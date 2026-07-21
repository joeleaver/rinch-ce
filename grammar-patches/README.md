# Grammar forks

tree-sitter grammars do not build for `wasm32-unknown-unknown` as published. We
carry small forks that fix that, and we maintain them ourselves rather than
upstreaming — so this directory, not a set of open PRs, is the record of what we
changed and why.

`grammars.tsv` is the source of truth. `scripts/grammar-forks.sh` reads it.

## Why the patches are needed

`wasm32-unknown-unknown` is the bare target: no libc, so no `<stdlib.h>`.
tree-sitter handles this by shipping a minimal freestanding header set in the
`tree-sitter-language` crate, which publishes its path as build metadata
(`DEP_TREE_SITTER_LANGUAGE_WASM_HEADERS`, via its `links` key).

Almost no grammar reads it. That is the whole fix for six of the eight.

Two needed more, because the header set is *minimal* — `string.h` declares only
`memcpy` and `strlen`, `wctype.h` only `iswalpha`, `ctype.h` only `isprint`, and
there is no `wchar.h` at all:

| Fork | Change |
|---|---|
| rust, typescript, javascript, json, toml, yaml | read the header path in `build.rs` |
| html | that, plus `strncpy` → `memcpy` and an ASCII `towupper` |
| markdown | `build.rs` was already fixed upstream; drop a dead `#include <wchar.h>`, add ASCII `isdigit`/`towlower`/`strcmp` |

`tree-sitter-css` needs no fork — it already builds unmodified.

Every C substitution is exact rather than approximate, and each is guarded by
`#if defined(__wasm32__) && !defined(__wasi__) && !defined(__EMSCRIPTEN__)` so
other targets keep using the libc implementations:

- `strncpy` → `memcpy` in html: `name_length` never exceeds the source length,
  so `strncpy`'s null-padding path was unreachable and the two are equivalent.
- The case folds are ASCII-only, and every call site folds ASCII — HTML tag
  names, the `script`/`style` end delimiters, ordered-list digits, and a
  markdown HTML-tag lookup table.

## Separately: the allocator

Getting a grammar to *compile* is not the whole story. tree-sitter's wasm bump
allocator is never initialized on this target and corrupts memory, silently,
above roughly 32 KB of input. `rinch-code-editor-syntax` installs a Rust-backed
allocator before any parsing (`src/ts/alloc.rs`), and the test suite keeps a
deliberately oversized parse as the canary. Do not remove it — anything smaller
passes even when the bug is present.

## Updating

```sh
./scripts/grammar-forks.sh check                    # what is behind
./scripts/grammar-forks.sh update tree-sitter-rust  # rebase patch onto the latest tag
./scripts/grammar-forks.sh verify                   # build every grammar for wasm32
```

`update` branches from the **tag matching the published crates.io version**,
never `master`, so the patched crate stays exactly the version cargo resolves.
It re-extracts the patch against the new base, verifies the wasm build, and only
then pushes.

If a patch stops applying, upstream changed the build script or scanner. The
script stops and tells you where the checkout is; fix it by hand, then
re-extract with the command it prints.

Note that after any force-push to a fork, `cargo update -p <crate>` is required
— cargo will otherwise keep building the previous cached git checkout, which
looks exactly like the fix not working.
