//! Desktop host for the code-editor component. This is the application side of
//! the peer-dependency split: it picks the backend (`desktop`) and calls `run`,
//! neither of which the library does.

use rinch::prelude::*;
use rinch_code_editor::CodeEditor;

const SAMPLE: &str = "fn main() {\n\
    \x20   let greeting = \"hello, world\";\n\
    \x20   // print it twice\n\
    \x20   for i in 0..2 {\n\
    \x20       println!(\"{greeting}\");\n\
    \x20   }\n\
}\n";

#[component]
fn app() -> NodeHandle {
    rsx! {
        div {
            style: "width:100%; height:100%; padding:24px; background:#131313; box-sizing:border-box;",
            div {
                style: "width:100%; height:100%; border:1px solid #000; border-radius:6px; overflow:hidden;",
                CodeEditor {
                    text: SAMPLE.to_string(),
                    language: "rust",
                }
            }
        }
    }
}

fn main() {
    run("rinch-ce demo", 900, 600, app);
}
