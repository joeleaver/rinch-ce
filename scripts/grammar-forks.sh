#!/usr/bin/env bash
#
# Maintain the tree-sitter grammar forks that let grammars build for
# wasm32-unknown-unknown. See grammar-patches/README.md for the why.
#
#   ./scripts/grammar-forks.sh check
#       Compare each pinned tag against the latest crates.io release and report
#       what is behind. Read-only.
#
#   ./scripts/grammar-forks.sh update <crate> [tag]
#       Re-apply that grammar's patch on top of a newer upstream tag, verify it
#       still compiles for wasm32, and push. Defaults to the tag matching the
#       latest crates.io release.
#
#   ./scripts/grammar-forks.sh verify
#       Build every grammar for wasm32 through the current patches.
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCH_DIR="$REPO_ROOT/grammar-patches"
MANIFEST="$PATCH_DIR/grammars.tsv"
WORK_DIR="${GRAMMAR_FORK_DIR:-$REPO_ROOT/../rinch-ce-forks}"
BRANCH="wasm32-sysroot"

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }

# Emit "crate upstream fork tag patch" per grammar, skipping comments.
manifest() { grep -vE '^\s*(#|$)' "$MANIFEST"; }

latest_version() {
    curl -sf "https://crates.io/api/v1/crates/$1" -H 'User-Agent: rinch-ce-grammar-forks' \
        | python3 -c 'import sys,json; print(json.load(sys.stdin)["crate"]["max_stable_version"])'
}

cmd_check() {
    printf '%-24s %-12s %-12s %s\n' CRATE PINNED LATEST STATUS
    local behind=0
    while IFS=$'\t' read -r crate upstream fork tag patch; do
        local latest status
        latest="$(latest_version "$crate" || echo '?')"
        if [[ "$tag" == "v$latest" ]]; then
            status="up to date"
        else
            status="BEHIND -> ./scripts/grammar-forks.sh update $crate"
            behind=$((behind + 1))
        fi
        printf '%-24s %-12s %-12s %s\n' "$crate" "$tag" "$latest" "$status"
    done < <(manifest)
    [[ $behind -eq 0 ]] && info "all grammars current" || info "$behind grammar(s) behind"
}

cmd_update() {
    local target_crate="${1:-}" requested_tag="${2:-}"
    [[ -n "$target_crate" ]] || die "usage: grammar-forks.sh update <crate> [tag]"

    local line
    line="$(manifest | awk -F'\t' -v c="$target_crate" '$1==c')"
    [[ -n "$line" ]] || die "unknown crate '$target_crate' (not in $MANIFEST)"
    IFS=$'\t' read -r crate upstream fork old_tag patch <<<"$line"

    local new_tag="$requested_tag"
    [[ -n "$new_tag" ]] || new_tag="v$(latest_version "$crate")"
    [[ "$new_tag" != "$old_tag" ]] || { info "$crate already on $new_tag"; return 0; }

    info "$crate: $old_tag -> $new_tag"
    mkdir -p "$WORK_DIR"
    local dir="$WORK_DIR/$(basename "$fork")"
    rm -rf "$dir"

    # Fetch the new upstream tag into our fork, then branch from it. Branching
    # from the tag rather than master keeps the patched crate at exactly the
    # version cargo resolves.
    git clone --quiet --depth 1 "git@github.com:$fork.git" "$dir"
    git -C "$dir" remote add upstream "https://github.com/$upstream.git"
    git -C "$dir" fetch --quiet --depth 1 upstream "refs/tags/$new_tag:refs/tags/$new_tag"
    git -C "$dir" checkout --quiet -B "$BRANCH" "refs/tags/$new_tag"

    if ! git -C "$dir" apply --3way "$PATCH_DIR/$patch"; then
        die "$crate: patch did not apply to $new_tag.
The grammar's build script or scanner changed upstream. Fix by hand in
  $dir
then re-extract with:
  git -C $dir diff refs/tags/$new_tag HEAD > $PATCH_DIR/$patch"
    fi

    git -C "$dir" add -A
    git -C "$dir" -c user.name="Joe Leaver" -c user.email="joe.leaver@lbmepublishing.com" \
        commit --quiet -m "build: support wasm32-unknown-unknown ($new_tag)"

    # Re-extract against the new base so the stored patch stays applicable, and
    # record the new tag before building — cargo resolves through the manifest.
    git -C "$dir" diff "refs/tags/$new_tag" HEAD > "$PATCH_DIR/$patch"
    sed -i "s|^\($crate\t$upstream\t$fork\t\)$old_tag|\1$new_tag|" "$MANIFEST"

    # Verify before publishing. A fork that does not build is worse than a stale
    # one, because Cargo.lock will happily pin it.
    info "$crate: verifying wasm32 build against the local checkout"
    ( cd "$REPO_ROOT" \
      && cargo update -p "$crate" --precise "${new_tag#v}" --quiet 2>/dev/null \
      || cargo update -p "$crate" --quiet 2>/dev/null || true )
    if ! cmd_verify; then
        die "$crate: $new_tag builds are broken; nothing was pushed.
The local checkout is at $dir if you want to investigate."
    fi

    git -C "$dir" push --force --quiet origin "$BRANCH"
    info "$crate updated to $new_tag and pushed"
    info "commit the changes to $MANIFEST, $PATCH_DIR/$patch and Cargo.lock"
}

cmd_verify() {
    info "building every grammar for wasm32-unknown-unknown"
    cd "$REPO_ROOT"
    # Unset any local include override so this proves the forks alone suffice.
    env -u CFLAGS_wasm32_unknown_unknown \
        cargo build -p rinch-code-editor-syntax --features all-languages \
        --target wasm32-unknown-unknown
    info "all grammars build for wasm32"
    cargo test -p rinch-code-editor-syntax --features all-languages --quiet
    info "tests pass"
}

case "${1:-}" in
    check)  cmd_check ;;
    update) shift; cmd_update "$@" ;;
    verify) cmd_verify ;;
    *)      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
