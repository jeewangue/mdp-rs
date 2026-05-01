#!/usr/bin/env bash
# Clean-room smoke gate for `mdp`. Builds the release binary, copies it to a
# pristine mktemp dir, and runs `mdp build` against a synthesised fixture
# exercising the fragile paths: nested directories, index.md vs README
# fallback, draft chapters, Hangul titles, KaTeX math, code fences.
#
# Runs from outside the repo cwd so a missed asset embed (rust-embed has no
# Cargo dep tracking by default — see build.rs) or a stale local reference
# can't be silently masked by files in the dev tree.
#
# Exit codes:  0 = pass, 1 = real failure, 77 = skip (toolchain missing).
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MDP_BIN="${MDP_BIN:-$REPO_DIR/target/release/mdp}"

if [[ ! -x "$MDP_BIN" ]]; then
  echo "error: $MDP_BIN missing — run 'cargo build --release' first." >&2
  exit 1
fi

# Pre-flight tools. NEVER call `mdp install-deps` from smoke (it runs
# `cargo install`). When the env is unprepared we exit 77 so CI can flag the
# skip explicitly instead of hiding it as success.
need_tools=(mdbook mdbook-katex mdbook-mermaid mdbook-plantuml mdbook-pagetoc)
missing=()
for t in "${need_tools[@]}"; do
  command -v "$t" >/dev/null 2>&1 || missing+=("$t")
done
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "skip: missing tools (${missing[*]}). run 'mdp install-deps' to enable smoke." >&2
  exit 77
fi

WORK="$(mktemp -d -t mdp-smoke.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

# Stage the binary in the clean root and leave the repo cwd. CRITICAL: any
# relative-path lookup that quietly resolves against $REPO_DIR's tree would
# mask exactly the bug class this gate is meant to catch.
cp "$MDP_BIN" "$WORK/mdp"
cd "$WORK"
unset CARGO_MANIFEST_DIR CARGO_TARGET_DIR

# ---- materialise fixture ---------------------------------------------------
FIX="$WORK/fixture"
mkdir -p \
  "$FIX/ko" \
  "$FIX/docs" \
  "$FIX/subdir/nested" \
  "$FIX/draft"

cat >"$FIX/index.md" <<'EOF'
# Smoke Root
EOF

cat >"$FIX/top.md" <<'EOF'
# Top Page

```rust
fn main() { println!("hello"); }
```
EOF

cat >"$FIX/ko/index.md" <<'EOF'
# 한글 제목

Hangul title regression — the SUMMARY URL must preserve UTF-8 bytes
without %-encoding the Hangul, and the rendered HTML must not mojibake.
EOF

# README fallback path (no index.md at this level).
cat >"$FIX/docs/README.md" <<'EOF'
# Docs Home

This dir intentionally has no `index.md`; SUMMARY should fall back to
README.md as the dir's entry chapter.
EOF

cat >"$FIX/docs/detail.md" <<'EOF'
# Detail
EOF

cat >"$FIX/subdir/index.md" <<'EOF'
# Sub Section
EOF

cat >"$FIX/subdir/nested/leaf.md" <<'EOF'
# Nested Leaf

A leaf with KaTeX: $E = mc^2$.

```python
print("nested")
```
EOF

cat >"$FIX/draft/orphan.md" <<'EOF'
# Orphan

This dir has no index.md or README. SUMMARY must emit a draft chapter
(empty link) for the parent dir.
EOF

# ---- build ----------------------------------------------------------------
"$WORK/mdp" build "$FIX" -o "$WORK/out" >"$WORK/build.log" 2>&1 || {
  echo "smoke FAIL: mdp build exited non-zero" >&2
  cat "$WORK/build.log" >&2
  exit 1
}

OUT="$WORK/out"

fail() {
  echo "smoke FAIL: $*" >&2
  exit 1
}

# A1: hierarchical sidebar (nested <ol class="section">).
grep -q '<ol class="section">' "$OUT/toc.html" \
  || fail "no <ol class=\"section\"> in toc.html (sidebar flat?)"

# A2/A3: breadcrumb assets actually shipped to theme/.
ls "$OUT/theme/" 2>/dev/null | grep -q '^mdp-breadcrumb.*\.css$' \
  || fail "mdp-breadcrumb.css missing from theme/"
ls "$OUT/theme/" 2>/dev/null | grep -q '^mdp-breadcrumb.*\.js$' \
  || fail "mdp-breadcrumb.js missing from theme/"

# A4: rendered page references the breadcrumb asset.
grep -q 'mdp-breadcrumb' "$OUT/subdir/nested/leaf.html" \
  || fail "leaf page does not reference mdp-breadcrumb asset"

# A5: Hangul title renders raw UTF-8 (no mojibake).
grep -q '한글 제목' "$OUT/ko/index.html" \
  || fail "Hangul title missing or mojibake'd in ko/index.html"

# A6: README fallback wins when index.md absent.
grep -q 'Docs Home' "$OUT/docs/index.html" 2>/dev/null \
  || grep -q 'Docs Home' "$OUT/docs/README.html" 2>/dev/null \
  || fail "README fallback page missing under docs/"

# A7: draft chapter renders (mdbook idiom: <li class="...affix..."> or
#     similar without an <a href>).
grep -qiE 'draft|affix' "$OUT/toc.html" \
  || fail "no draft / affix marker in toc.html"

# A8: KaTeX rendered the math snippet.
grep -q 'class="katex' "$OUT/subdir/nested/leaf.html" \
  || fail "KaTeX did not render \$E=mc^2\$ on leaf page"

# A9: every theme asset referenced in the rendered <head> exists on disk.
referenced="$(grep -oE 'theme/[A-Za-z0-9._/-]+\.(css|js)' "$OUT/index.html" | sort -u)"
while IFS= read -r ref; do
  [[ -z "$ref" ]] && continue
  test -f "$OUT/$ref" \
    || fail "theme asset referenced but missing on disk: $ref"
done <<<"$referenced"

# A10: no stranded SUMMARY.md.tmp from the atomic-write path.
if find "$WORK" -name 'SUMMARY.md.tmp' | grep -q .; then
  fail "stranded SUMMARY.md.tmp left behind"
fi

# A11: build log clean of panics / unhandled errors. mdbook prints "[ERROR]"
# on real failures; a successful build is otherwise quiet on stderr.
if grep -qiE '^thread .* panicked|panicked at|\[ERROR\]' "$WORK/build.log"; then
  fail "build log contains panic / [ERROR] lines"
fi

echo "smoke ok: $(wc -l < "$WORK/build.log") build-log lines, all assertions pass"
