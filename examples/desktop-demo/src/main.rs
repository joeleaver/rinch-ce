//! Desktop host for the code-editor component. This is the application side of
//! the peer-dependency split: it picks the backend (`desktop`) and calls `run`,
//! neither of which the library does.

use rinch::prelude::*;
use rinch_code_editor::CodeEditor;

// A document taller than the viewport, so scrolling and virtualization are
// exercised. Line numbers are called out in comments to make the visible window
// obvious while scrolling. A raw string (not `\`-continuations, which strip the
// leading whitespace off every line) so the sample keeps its indentation.
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

// Lines wider than the viewport, so horizontal scrolling has something to do.
// Press End on one to watch the caret reveal drag the view right. (line 68)
const WIDE: &str = "the quick brown fox jumps over the lazy dog, and then keeps going well past the right-hand edge of any reasonable window, because that is the entire point of this line";

fn a_long_chain(values: &[i32]) -> Vec<String> {
    values.iter().copied().filter(|v| *v % 2 == 0).map(|v| v * 3).filter(|v| *v > 10).map(|v| format!("value {v} survived the whole chain")).collect::<Vec<_>>()
}
"#;

/// The sample, plus one line long enough to trip `stopRenderingLineAfter`
/// (10000 chars). Built at runtime rather than pasted in, because a literal
/// 15000-character line in this file would be unreadable and rustfmt would
/// fight it. It renders capped, with a "Show more" chip; clicking at or past
/// the cap lifts it for the rest of the session.
fn sample() -> String {
    let mut s = String::from(SAMPLE);
    s.push_str("\n// The next line is 15,000 characters. It draws the first 10,000. (line 74)\n");
    s.push_str("const HUGE: &str = \"");
    s.push_str(&"abcdefghij".repeat(1_500));
    s.push_str("\";\n");
    s
}

#[component]
fn app() -> NodeHandle {
    rsx! {
        div {
            style: "width:100%; height:100%; padding:24px; background:#131313; box-sizing:border-box;",
            div {
                style: "width:100%; height:100%; border:1px solid #000; border-radius:6px; overflow:hidden;",
                CodeEditor {
                    text: sample(),
                    language: "rust",
                }
            }
        }
    }
}

fn main() {
    run("rinch-ce demo", 900, 600, app);
}
