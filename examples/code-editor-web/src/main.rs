//! Browser host for the code-editor component.
//!
//! The web half of the peer-dependency split, and the exact mirror of
//! `examples/desktop-demo`: that one names rinch's `desktop` feature and calls
//! `run`, this one brings `rinch-web` and calls `mount`. The library in between
//! names no backend at all, and the `CodeEditor` element below is
//! character-for-character the one the desktop demo mounts — which is the point
//! of the example. If the component ever needs a `#[cfg]` to work here, the
//! split has sprung a leak.
//!
//! Stable element ids (`page`, `editor-host`) make the page Playwright-assertable.
//!
//! # Running it
//!
//! ```text
//! cd examples/code-editor-web
//! trunk serve --port 8099        # then open http://127.0.0.1:8099/
//! ```
//!
//! Not `cargo run`: this package is outside the workspace and builds for
//! `wasm32-unknown-unknown`. `trunk` compiles it, runs wasm-bindgen, and serves
//! `index.html` — whose CSS the editor genuinely depends on, for the reason
//! documented there.

use rinch::prelude::*;
use rinch_code_editor::CodeEditor;
use rinch_core::element::ThemeProviderProps;
use wasm_bindgen::prelude::*;

// The same document the desktop demo uses, so the two can be compared line for
// line — taller than the viewport, so scrolling and virtualization are
// exercised, with line numbers called out in comments to make the visible
// window obvious while scrolling. A raw string (not `\`-continuations, which
// strip the leading whitespace off every line) so the sample keeps its
// indentation.
const SAMPLE: &str = r#"//! rinch-ce virtualization demo — scroll with the wheel or the arrow keys.

use std::collections::HashMap;
use std::fmt::{self, Display};

/// A point in two dimensions. (line 6)
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    const ORIGIN: Point = Point { x: 0.0, y: 0.0 };

    fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    fn distance_to(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt() // line 23
    }
}

impl Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

/// Tally how often each word appears. (line 33)
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
        _ => fibonacci(n - 1) + fibonacci(n - 2), // line 46
    }
}

fn main() {
    let a = Point::new(3.0, 4.0);
    let b = Point::ORIGIN;
    println!("distance = {}", a.distance_to(&b));

    let counts = word_counts("the cat sat on the mat");
    for (word, n) in &counts {
        println!("{word}: {n}"); // line 57
    }

    for i in 0..10 {
        println!("fib({i}) = {}", fibonacci(i));
    }
}
"#;

#[component]
fn app() -> NodeHandle {
    rsx! {
        div {
            id: "page",
            style: "width:100%; height:100%; padding:24px; background:#131313; box-sizing:border-box;",
            div {
                id: "editor-host",
                style: "width:100%; height:100%; border:1px solid #000; border-radius:6px; overflow:hidden;",
                CodeEditor {
                    text: SAMPLE.to_string(),
                    language: "rust",
                }
            }
        }
    }
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    // The editor supplies its own dark theme for the text; this only sets the
    // chrome around it, and dark keeps the page from flashing white first.
    let theme = ThemeProviderProps {
        dark_mode: true,
        ..Default::default()
    };
    rinch_web::mount(theme, app);
}

fn main() {
    // Entry point is `start()` via #[wasm_bindgen(start)].
}
