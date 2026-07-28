# rinch-ce

A Monaco-parity code editor for the [rinch](https://github.com/joeleaver/rinch)
GUI framework — text engine, syntax highlighting, view layer, and CRDT
collaborative editing, as a component you drop into a rinch application.

```rust
use rinch::prelude::*;
use rinch_code_editor::CodeEditor;

#[component]
fn App() -> NodeHandle {
    rsx! {
        CodeEditor {
            text: "fn main() {}",
            language: "rust",
        }
    }
}
```

## Building

```sh
git clone https://github.com/joeleaver/rinch-ce
cd rinch-ce
cargo build --release
```

That is the whole of it. `rinch` is not on crates.io, so it comes from git —
tracking `main`, since this editor is developed alongside it — as do the forked
`wgpu`/`winit` that rinch's desktop backend needs and the forked tree-sitter
grammars. Cargo fetches all of them.

**Build in release.** Debug builds are usable but slow enough to change how the
editor feels, and the test suite runs in seconds under `--release` versus
minutes without it.

### Using it in your own application

Add the editor and rinch from the same source it tracks:

```toml
[dependencies]
rinch-code-editor = { git = "https://github.com/joeleaver/rinch-ce" }
rinch = { git = "https://github.com/joeleaver/rinch", branch = "main", features = ["desktop"] }
```

Your application and this crate **must resolve to the same `rinch`** — same
source, same commit — or Cargo treats them as two different crates and a
`rinch::Signal` from one is not the same type as from the other. Matching
`branch = "main"` is what makes Cargo unify them into one. If you see
mismatched-type errors, run `cargo tree -d rinch`. See
[`crates/rinch-code-editor/README.md`](crates/rinch-code-editor/README.md) for
the full explanation of why rinch is a peer dependency.

A desktop application additionally needs rinch's `[patch.crates-io]` block for
its forked `wgpu`/`winit`, because a `[patch]` only takes effect from the
top-level workspace. Copy the block from this repo's `Cargo.toml`. A library
that leaves rinch on `default-features = false` needs none of it — the forks sit
behind rinch's `desktop`/`gpu` features.

### Building against a local rinch checkout

To test an unmerged rinch branch without editing the manifest, drop this in
`.cargo/config.toml` (gitignored):

```toml
[patch."https://github.com/joeleaver/rinch"]
rinch = { path = "../rinch/crates/rinch" }
rinch-core = { path = "../rinch/crates/rinch-core" }
```

Delete it to go back to the pinned rev. Please don't turn the manifest entries
back into path dependencies — that is what made this repo unbuildable for
anyone who had not also cloned rinch beside it.

## Layout

Only `rinch-code-editor` knows rinch exists. Everything correctness-critical
sits in three rinch-free crates below it, so it is testable without a GUI —
which is why the great majority of the ~400 tests live there.

| Crate | What it owns |
|---|---|
| `rinch-code-editor-buffer` | Rope text, multi-cursor selections, undo history |
| `rinch-code-editor-syntax` | Monaco-compatible packed tokens, themes, tree-sitter backend |
| `rinch-code-editor-model` | One edit API over buffer + selections + history + highlighter; monospace grid geometry; cursor motion |
| `rinch-code-editor` | The rinch component: DOM, virtualized rendering, input, overlays |
| `rinch-code-editor-collab` | CRDT document and presence over yrs / the y-sync protocol — **transport-agnostic**, speaks only bytes |

The dependency arrow only ever points downward, and `rinch-code-editor-collab`
is *not* a dependency of the view: an application is the only place that names
both. See [`crates/rinch-code-editor/README.md`](crates/rinch-code-editor/README.md)
for why rinch is a peer dependency and what breaks if two copies of it end up in
the tree.

## Running the demos

```sh
# The editor on its own — highlighting, virtualized scrolling, mouse, clipboard.
cargo run --release -p rinch-code-editor-desktop-demo

# Two editors sharing one CRDT document, in one process.
cargo run --release --bin collab-demo

# Remote presence rendering alone, driven by scripted peers.
cargo run --release --bin presence-demo
```

Collaborative editing across *processes*, over a real websocket — a reference
relay plus two clients. Three terminals:

```sh
cargo run --release -p rinch-code-editor-relay
cargo run --release -p rinch-code-editor-net-demo -- Ada --seed
cargo run --release -p rinch-code-editor-net-demo -- Grace
```

The relay is an ordinary `CollabDoc` with one session per client and no editor
attached — the collab crate never opens a socket, so the network lives entirely
in the example.

For the browser (`examples/code-editor-web`, which is outside the workspace and
carries its own lockfile):

```sh
cd examples/code-editor-web && trunk serve --port 8099
```

## Status

Working: rope buffer with multi-cursor editing and undo, tree-sitter
highlighting (9 languages, each behind its own `lang-*` feature so you compile
in only the grammars you use), fixed-height vertical virtualization, horizontal
scrolling with caret reveal and drag auto-scroll, mouse selection with Monaco's
multi-click gestures, clipboard with Monaco's line semantics, caret blink, and
collaborative editing with remote carets and name labels — in-process and over a
websocket.

Not yet: code folding, minimap, diagnostics, LSP, and IME composition (which is
[blocked upstream](https://github.com/joeleaver/rinch/issues/176)).

Design decisions follow Monaco unless rinch forces otherwise, and where they
deviate the reason is recorded next to the code.

## Grammar forks

tree-sitter grammars do not build for `wasm32-unknown-unknown` as published, so
eight of them are forked. [`grammar-patches/`](grammar-patches/) is the record of
what changed and why; `scripts/grammar-forks.sh` maintains the forks from
`grammar-patches/grammars.tsv`.

## License

MIT OR Apache-2.0, at your option.
