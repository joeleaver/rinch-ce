# rinch-code-editor

A Monaco-parity code editor component for the [rinch](https://github.com/joeleaver/rinch)
GUI framework. Drop it into an existing rinch application to get a real editor:
syntax highlighting, multi-cursor editing, undo, and (in progress) the full set
of Monaco features.

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

## rinch is a *peer* dependency

This crate depends on `rinch`, and your application depends on `rinch` too.
That is a diamond, **not** a dependency cycle: `rinch` is upstream and never
refers back to this crate — the arrow only ever points from the editor *to*
rinch. The shape is exactly React ↔ a React component library: your app and the
component both depend on the framework; the framework depends on neither.

The one rule that keeps the diamond sound is that **your app and this crate must
resolve to the same `rinch` instance.** If they end up on two different versions
— or the same version from two different *sources* — Cargo treats them as
distinct crates and a `rinch::Signal` from one will not be the same type as from
the other. So:

- **We depend on `rinch` with a wide version range and no default features.**
  The editor never turns on a rendering backend; picking `desktop` or `web` is
  the application's decision, exactly as a peer dependency should behave.
- **We do not vendor or re-`[patch]` rinch.** rinch's desktop builds require a
  `[patch.crates-io]` block for its forked `wgpu`/`winit`; that belongs in the
  *application's* manifest (any desktop rinch app already carries it) and, being
  a `[patch]`, it applies workspace-wide and unifies the editor's `rinch` with
  the app's. Bundling our own copy would fork the framework and break type
  identity.

If you see errors about mismatched `rinch` types, the cause is almost always two
`rinch` instances in the tree — check `cargo tree -d rinch`.

## The stack beneath this crate

The editor is the only crate here that depends on rinch. Everything hard sits in
three rinch-free crates below it, so the correctness-critical logic is testable
without a GUI:

- **`rinch-code-editor-buffer`** — rope text, multi-cursor selections, undo.
- **`rinch-code-editor-syntax`** — Monaco-compatible packed tokens, themes, and
  an optional tree-sitter backend.
- **`rinch-code-editor-model`** — the orchestration (one edit API over buffer +
  selections + history + highlighter) plus the monospace grid geometry and
  cursor motion.

## License

MIT OR Apache-2.0.
