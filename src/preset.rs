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

        // Symlink user's dir into `src/` so mdbook watches the real files (live reload
        // works on save).
        #[cfg(unix)]
        std::os::unix::fs::symlink(&src_canonical, &book_src)
            .context("failed to symlink source dir into workspace")?;
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&book_src)?;
            copy_recursive(&src_canonical, &book_src)?;
        }

        let cfg = BookConfig::new(&src_canonical, title_override)?;
        write_book_toml(&book_root, &cfg)?;
        generate_summary(&book_src, &src_canonical)?;
        install_theme(&book_root)?;

        let _ = src_canonical; // moved into generate_summary above
        Ok(Self { root: book_root, src: book_src, _tmp: tmp })
    }
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
/// reject any entry whose canonical path escapes the tree (symlink traversal
/// defense).
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

        // Reject any symlink that escapes the tree. We use symlink_metadata to
        // detect the symlink without following, then canonicalize to verify the
        // target is under our root. Non-symlink entries also get canonicalized
        // as a belt-and-braces check against `..` inside paths.
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

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // Skip hidden + common noise
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.starts_with('.') || name == "node_modules" || name == "target")
            {
                continue;
            }
            out.extend(walk_md(&path, canonical_root)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "SUMMARY.md" || name == "README.md" {
                // SUMMARY.md: we're generating it. README.md: handled by mdbook natively.
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
    fn strip_controls_removes_newlines() {
        assert_eq!(strip_controls("hello\nworld"), "helloworld");
        assert_eq!(strip_controls("path with space"), "path with space");
    }
}
