//! Workspace preparation: copy the user's directory into a tmpdir, generate book.toml
//! and SUMMARY.md, and drop theme files into `theme/`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::config::BookConfig;
use crate::theme::Assets;

pub struct Workspace {
    /// Root of the prepared mdbook project (tmpdir root).
    pub root: PathBuf,
    /// `root/src/` — symlink or copy of the user's dir.
    pub src: PathBuf,
    /// RAII tempdir — dropped when Workspace goes out of scope, cleaning up the
    /// prepared files. Holding it prevents `/tmp/mdp-*` directories from leaking.
    _tmp: TempDir,
}

impl Workspace {
    pub fn prepare(src_dir: &Path, title_override: Option<String>) -> Result<Self> {
        let tmp = tempfile::Builder::new().prefix("mdp-").tempdir()?;
        let book_root = tmp.path().to_path_buf();
        let book_src = book_root.join("src");

        let src_canonical = src_dir.canonicalize().context("source dir must exist")?;

        // Mirror ONLY `.md` files from the user's dir into `src/` as per-file
        // symlinks.
        //
        // Earlier versions symlinked the entire user dir. That's cheaper but
        // mdbook walks the whole src tree WITHOUT honoring our exclusion list,
        // so node_modules self-loops (`foo/node_modules/foo -> ../..`, common
        // in npm peer-dep setups) crashed the build with "Too many levels of
        // symbolic links". Per-file mirroring keeps mdbook scoped to files we
        // whitelisted while inotify still fires on edits because it follows
        // symlinks.
        std::fs::create_dir_all(&book_src)?;
        let mut mirrored = 0usize;
        mirror_md_files(&src_canonical, &src_canonical, &book_src, &mut mirrored)?;
        tracing::debug!("mirrored {mirrored} .md files into {}", book_src.display());

        let cfg = BookConfig::new(&src_canonical, title_override)?;
        write_book_toml(&book_root, &cfg)?;
        generate_summary(&book_src, &src_canonical)?;
        install_theme(&book_root)?;

        Ok(Self { root: book_root, src: book_src, _tmp: tmp })
    }
}

/// Recursively mirror `.md` files from `src` into `dst`, preserving relative
/// paths. Applies the common exclusion list at the directory level.
///
/// On unix, files become symlinks (cheap, live-reload friendly — inotify
/// follows symlinks). On Windows we fall back to copy.
fn mirror_md_files(
    src: &Path,
    canonical_root: &Path,
    dst_root: &Path,
    mirrored: &mut usize,
) -> Result<()> {
    let rd = match std::fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();

        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Block symlinks that escape the tree — their target could be anywhere.
        if md.file_type().is_symlink() {
            let resolved = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !resolved.starts_with(canonical_root) {
                tracing::warn!(
                    "skipping symlink that escapes source tree: {}",
                    path.display()
                );
                continue;
            }
        }

        // `is_dir()` on a symlink follows; we already bounded it above.
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && is_excluded_dir(name)
            {
                continue;
            }
            mirror_md_files(&path, canonical_root, dst_root, mirrored)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let rel = path.strip_prefix(canonical_root).unwrap_or(&path);
            let dst_path = dst_root.join(rel);
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if dst_path.exists() {
                continue;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&path, &dst_path)?;
            #[cfg(not(unix))]
            std::fs::copy(&path, &dst_path)?;
            *mirrored += 1;
        }
    }
    Ok(())
}

/// Common noise directories we never mirror into the mdbook workspace. Kept
/// conservative — we only exclude directories that virtually never hold
/// user-authored markdown.
pub(crate) fn is_excluded_dir(name: &str) -> bool {
    // Hidden dirs (`.foo`) blanket-excluded — covers `.git`, `.svn`, `.hg`,
    // `.idea`, `.vscode`, `.claude`, `.cache`, `.venv`, `.next`, `.nuxt`,
    // `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.bin`, `.wrangler`.
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"              // rust
            | "dist"
            | "build"
            | "out"
            | "venv"
            | "__pycache__"
            | "vendor"              // go / php
            | "Pods"                // cocoapods
            | "DerivedData"         // xcode
            | "bower_components"
            | "coverage"
    )
}

fn write_book_toml(root: &Path, cfg: &BookConfig) -> Result<()> {
    let tmpl_bytes = Assets::get("book-toml/book.toml.tmpl")
        .context("embedded book.toml.tmpl missing")?
        .data;
    let tmpl = std::str::from_utf8(tmpl_bytes.as_ref())?;

    // Fully escape for TOML via the `toml` crate — this handles control chars,
    // surrogate halves, and quotes correctly (unlike the hand-rolled escaper
    // which only covered `\` and `"`).
    //
    // Note the substitutions land INSIDE `"..."` in the template, so we strip the
    // surrounding quotes from the serialized value.
    let rendered = tmpl
        .replace("{{TITLE}}", &toml_string_body(&cfg.title))
        .replace("{{AUTHOR}}", &toml_string_body(&cfg.author))
        .replace("{{PLANTUML_SERVER}}", &toml_string_body(&cfg.plantuml_server))
        // SRC_DIR is a comment on line 2 — any newline in the path would escape the
        // comment. Strip CR/LF + other control chars, same treatment.
        .replace("{{SRC_DIR}}", &strip_controls(&cfg.src_dir_display));
    std::fs::write(root.join("book.toml"), rendered)?;
    Ok(())
}

/// Public re-export for callers outside this module that need the same escaping
/// (e.g. commands::pdf when it appends preprocessor config to book.toml).
pub fn toml_string_body_public(s: &str) -> String {
    toml_string_body(s)
}

/// Escape `s` as a TOML basic string body — i.e. the bit that goes INSIDE
/// `"..."` in the template. Handles all TOML-required escapes per
/// https://toml.io/en/v1.0.0#string:
///   \b, \t, \n, \f, \r, \", \\, and \uXXXX for other control chars.
/// Non-control, non-quote characters (including UTF-8 beyond ASCII) pass through
/// unchanged.
///
/// We hand-roll this rather than using `toml::Value::String::to_string()` because
/// the `toml` crate may pick LITERAL string (single-quoted) or MULTI-LINE basic
/// string output depending on what's most compact — but our template has hard-
/// coded `"..."` surroundings, so we need a basic-string BODY specifically.
fn toml_string_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\x08' => out.push_str(r"\b"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\x0c' => out.push_str(r"\f"),
            '\r' => out.push_str(r"\r"),
            c if c.is_control() => {
                // \uXXXX for BMP, \UXXXXXXXX for higher planes. TOML supports both.
                let n = c as u32;
                if n <= 0xFFFF {
                    out.push_str(&format!("\\u{n:04X}"));
                } else {
                    out.push_str(&format!("\\U{n:08X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Remove control chars (< 0x20 except space) for use inside a TOML comment line.
fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || *c == ' ').collect()
}

/// Scan `src/` for `.md` files and write `SUMMARY.md` if one doesn't exist.
///
/// mdbook REQUIRES SUMMARY.md. If the user already has one we respect it; otherwise we
/// auto-generate from the directory structure (alphabetical, index.md first).
///
/// Symlinks in the tree that resolve OUTSIDE `canonical_root` are skipped — this
/// prevents a malicious dir from exposing arbitrary `.md` files on the filesystem
/// via the served preview.
fn generate_summary(src: &Path, canonical_root: &Path) -> Result<()> {
    let summary = src.join("SUMMARY.md");
    if summary.exists() {
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = walk_md(src, canonical_root)?;
    entries.sort_by(|a, b| {
        let ai = a.file_name().and_then(|n| n.to_str()) == Some("index.md");
        let bi = b.file_name().and_then(|n| n.to_str()) == Some("index.md");
        match (ai, bi) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });

    let mut out = String::from("# Summary\n\n");
    for entry in entries {
        let rel = entry.strip_prefix(src).unwrap_or(&entry);
        let title = read_title(&entry).unwrap_or_else(|| {
            rel.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_string()
        });
        // Escape `]` and `)` in filenames so they can't break the link syntax.
        let safe_title = title.replace(['[', ']'], "");
        out.push_str(&format!("- [{safe_title}](./{})\n", rel.display()));
    }

    // `summary` lives at `book_src/SUMMARY.md`. Since `book_src` is a symlink to
    // the user's real source dir, writing here DOES touch the user's dir. If that
    // fails (read-only fs, permission), the warning tells them to create one
    // manually. See TODO in commit log for moving this to a shadow location.
    match std::fs::write(&summary, out) {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!(
                "could not write auto-generated SUMMARY.md to {}: {e} — add one manually",
                summary.display()
            );
            Err(e.into())
        }
    }
}

/// Walk `dir` recursively, collecting `.md` files. `canonical_root` is used to
/// reject symlinks whose target escapes the tree. Shares the exclusion list
/// with `mirror_md_files` so SUMMARY.md generation and workspace mirroring
/// see exactly the same files.
fn walk_md(dir: &Path, canonical_root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();

        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_symlink() {
            let resolved = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !resolved.starts_with(canonical_root) {
                tracing::warn!("skipping symlink that escapes source tree: {}", path.display());
                continue;
            }
        }

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && is_excluded_dir(name)
            {
                continue;
            }
            out.extend(walk_md(&path, canonical_root)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "SUMMARY.md" || name == "README.md" {
                continue;
            }
            out.push(path);
        }
    }
    Ok(out)
}

fn read_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn install_theme(root: &Path) -> Result<()> {
    let theme_dir = root.join("theme");
    std::fs::create_dir_all(&theme_dir)?;

    // Extract julian.jee CSS files with conventional names (flat, under theme/).
    for (src_path, dst_name) in [
        ("themes/julian.jee/css/variables.css", "julian-jee-variables.css"),
        ("themes/julian.jee/css/general.css", "julian-jee-general.css"),
    ] {
        let data = Assets::get(src_path)
            .ok_or_else(|| anyhow::anyhow!("embedded asset missing: {src_path}"))?
            .data;
        std::fs::write(theme_dir.join(dst_name), data.as_ref())?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_string_body_escapes_quotes_and_backslash() {
        assert_eq!(toml_string_body("hello"), "hello");
        assert_eq!(toml_string_body(r#"he"llo"#), r#"he\"llo"#);
        assert_eq!(toml_string_body(r"he\llo"), r"he\\llo");
    }

    #[test]
    fn toml_string_body_escapes_control_chars() {
        // Newline must be escaped so it can't break out of a basic string.
        assert_eq!(toml_string_body("a\nb"), r"a\nb");
        assert_eq!(toml_string_body("a\rb"), r"a\rb");
        assert_eq!(toml_string_body("a\tb"), r"a\tb");
    }

    #[test]
    fn toml_string_body_rejects_toml_injection() {
        // A crafted title trying to break out of the string and inject a new key
        // must remain inside the string when serialized.
        let evil = r#"evil"
[output.html]
additional-js = ["http://evil/x.js"]
#"#;
        let escaped = toml_string_body(evil);
        // No raw newline or unescaped quote.
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains("\"\n"));
    }

    #[test]
    fn toml_string_body_escapes_all_ascii_controls() {
        // Every byte below 0x20 (except the ones with their own escape) must
        // come back as a \uXXXX sequence so TOML parses it. 0x7F (DEL) also
        // should be encoded.
        for n in 0u32..=0x1f {
            let s = String::from(char::from_u32(n).unwrap());
            let escaped = toml_string_body(&s);
            // must not contain the raw control char
            assert!(
                !escaped.chars().any(|c| c.is_control() && c != ' '),
                "{n:#x} not escaped: {escaped:?}"
            );
        }
        // 0x7F (DEL) is a control char per char::is_control(); we escape it
        // conservatively even though TOML's grammar doesn't strictly require it.
        let del_escaped = toml_string_body("\x7f");
        assert_eq!(del_escaped, "\\u007F");
    }

    #[test]
    fn toml_string_body_roundtrips_via_toml_crate() {
        // Anything we escape must parse back to the original string when the
        // full TOML `"..."` wrapper is applied.
        for input in [
            "plain",
            "한글",
            "emoji 🌸",
            r#"q"uo"tes"#,
            "line\nbreak",
            "\tindented",
            "with \\ backslash",
            "",
        ] {
            let wrapped = format!("v = \"{}\"", toml_string_body(input));
            let parsed: toml::Value = toml::from_str(&wrapped)
                .unwrap_or_else(|e| panic!("input {input:?} failed to parse: {e}"));
            let v = parsed["v"].as_str().expect("v must be a string");
            assert_eq!(v, input, "round-trip mismatch for {input:?}");
        }
    }

    #[test]
    fn toml_string_body_handles_high_plane_unicode() {
        // Printable high-plane chars (emoji) pass through unchanged.
        assert_eq!(toml_string_body("🌸"), "🌸");
        assert_eq!(toml_string_body("한"), "한");
        // U+1F338 cherry blossom is in the astral plane — TOML allows raw
        // astral chars in basic strings. Roundtrip check.
        let wrapped = format!("v = \"{}\"", toml_string_body("🌸 한글 and 🚀"));
        let parsed: toml::Value = toml::from_str(&wrapped).unwrap();
        assert_eq!(parsed["v"].as_str().unwrap(), "🌸 한글 and 🚀");
    }

    #[test]
    fn strip_controls_removes_newlines() {
        assert_eq!(strip_controls("hello\nworld"), "helloworld");
        assert_eq!(strip_controls("path with space"), "path with space");
    }

    #[test]
    fn strip_controls_preserves_utf8() {
        assert_eq!(strip_controls("안녕 world 🌸"), "안녕 world 🌸");
    }

    #[test]
    fn strip_controls_removes_tab_and_null() {
        assert_eq!(strip_controls("a\tb\0c"), "abc");
    }

    #[test]
    fn walk_md_skips_hidden_and_noise_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // shape:
        //   root/
        //     visible.md
        //     README.md      (skipped)
        //     .hidden/inside.md
        //     node_modules/blah.md
        //     target/debug/foo.md
        //     sub/nested.md
        std::fs::write(root.join("visible.md"), "# V").unwrap();
        std::fs::write(root.join("README.md"), "# R").unwrap();
        for d in [".hidden", "node_modules", "target/debug"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("hidden.md"), "# H").unwrap();
        }
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/nested.md"), "# N").unwrap();

        let mut found = walk_md(root, root).unwrap();
        found.sort();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["sub/nested.md".to_string(), "visible.md".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn walk_md_rejects_symlink_outside_root() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "# S").unwrap();

        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();
        std::fs::write(root.join("inside.md"), "# I").unwrap();
        // Symlink from root → outside secret.md
        std::os::unix::fs::symlink(outside.path().join("secret.md"), root.join("link.md")).unwrap();

        let canonical_root = root.canonicalize().unwrap();
        let found: Vec<_> = walk_md(root, &canonical_root)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // link.md MUST be rejected; inside.md allowed.
        assert_eq!(found, vec!["inside.md".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn walk_md_follows_symlink_inside_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();
        std::fs::write(root.join("real.md"), "# R").unwrap();
        // symlink → sibling in same tree is fine.
        std::os::unix::fs::symlink(root.join("real.md"), root.join("alias.md")).unwrap();

        let canonical_root = root.canonicalize().unwrap();
        let mut found: Vec<_> = walk_md(root, &canonical_root)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["alias.md".to_string(), "real.md".to_string()]);
    }

    #[test]
    fn is_excluded_dir_covers_common_noise() {
        for name in [
            "node_modules", "target", "dist", "build", "out",
            "venv", "__pycache__", "vendor", "Pods", "DerivedData",
            "bower_components", "coverage",
            ".git", ".svn", ".hg", ".idea", ".vscode", ".claude",
            ".cache", ".venv", ".next", ".nuxt",
            ".pytest_cache", ".mypy_cache", ".ruff_cache",
            ".bin", ".wrangler", ".DS_Store",
        ] {
            assert!(super::is_excluded_dir(name), "{name} should be excluded");
        }
    }

    #[test]
    fn is_excluded_dir_allows_content_dirs() {
        for name in [
            "docs", "src", "notes", "posts", "chapters",
            "nodemodules",   // close but not the magic name
            "target-legacy", // prefix only
            "my-build",      // suffix only
            "anything",      // "target" substring but not whole name
        ] {
            assert!(!super::is_excluded_dir(name), "{name} should NOT be excluded");
        }
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_skips_excluded_dirs() {
        // Tree:
        //   root/keep.md
        //   root/sub/keep2.md
        //   root/node_modules/hidden.md  ← excluded
        //   root/target/in-target.md     ← excluded
        //   root/.git/config.md          ← excluded (hidden)
        //   root/.claude/settings.md     ← excluded (hidden)
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("keep.md"), "# K").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/keep2.md"), "# K2").unwrap();
        for d in ["node_modules", "target", ".git", ".claude"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("hidden.md"), "# H").unwrap();
        }
        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n).unwrap();
        assert_eq!(n, 2, "only keep.md and sub/keep2.md should be mirrored");
        assert!(dst.path().join("keep.md").exists());
        assert!(dst.path().join("sub/keep2.md").exists());
        assert!(!dst.path().join("node_modules").exists());
        assert!(!dst.path().join("target").exists());
        assert!(!dst.path().join(".git").exists());
        assert!(!dst.path().join(".claude").exists());
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_survives_symlink_loop() {
        // Reproduce the npm self-loop pattern:
        //   root/pkg/node_modules/pkg -> ../../pkg
        // If we don't exclude node_modules this would infinite-loop when mdbook
        // scans — but our mirror should skip it before mdbook ever sees it.
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("top.md"), "# T").unwrap();
        std::fs::create_dir_all(root.join("pkg/node_modules")).unwrap();
        std::fs::write(root.join("pkg/README.md"), "# R").unwrap();
        // Create the loop.
        std::os::unix::fs::symlink(
            "../../pkg",
            root.join("pkg/node_modules/pkg"),
        )
        .unwrap();

        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        // Must not panic / hang.
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n).unwrap();
        assert!(dst.path().join("top.md").exists());
        assert!(dst.path().join("pkg/README.md").exists());
        // node_modules was excluded — no recursion into the loop.
        assert!(!dst.path().join("pkg/node_modules").exists());
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_rejects_symlink_escaping_root() {
        // A symlink whose target is OUTSIDE canonical_root must be skipped even
        // if it ends with `.md`.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "# S").unwrap();
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("real.md"), "# R").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            root.join("escaping.md"),
        )
        .unwrap();

        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n).unwrap();
        assert!(dst.path().join("real.md").exists());
        assert!(
            !dst.path().join("escaping.md").exists(),
            "tree-escaping symlink should not be mirrored"
        );
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.md"), "# A").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let canonical = src.path().canonicalize().unwrap();
        let mut n1 = 0;
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n1).unwrap();
        let mut n2 = 0;
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n2).unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 0, "rerun should be a no-op when all files are already mirrored");
        assert!(dst.path().join("a.md").exists());
    }

    #[test]
    fn read_title_handles_leading_whitespace_and_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "   # With leading spaces\ncontent\n").unwrap();
        assert_eq!(read_title(tmp.path()).as_deref(), Some("With leading spaces"));

        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp2.path(), "").unwrap();
        assert_eq!(read_title(tmp2.path()), None);

        let tmp3 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp3.path(), "no heading line\nwith text\n").unwrap();
        assert_eq!(read_title(tmp3.path()), None);

        // H1 somewhere after content: we pick the first H1 we see.
        let tmp4 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp4.path(), "intro\n# Later H1\nbody\n").unwrap();
        assert_eq!(read_title(tmp4.path()).as_deref(), Some("Later H1"));
    }
}
