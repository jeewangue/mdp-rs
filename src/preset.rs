//! Workspace preparation: copy the user's directory into a tmpdir, generate book.toml
//! and SUMMARY.md, and drop theme files into `theme/`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::config::BookConfig;
use crate::theme::{Assets, extract_to};

pub struct Workspace {
    /// Root of the prepared mdbook project (tmpdir root).
    pub root: PathBuf,
    /// `root/src/` — symlink or copy of the user's dir.
    pub src: PathBuf,
}

impl Workspace {
    pub fn prepare(src_dir: &Path, title_override: Option<String>) -> Result<Self> {
        let tmp = tempfile::Builder::new().prefix("mdp-").tempdir()?.keep();
        let book_root = tmp;
        let book_src = book_root.join("src");

        std::fs::create_dir_all(&book_src)?;
        let src_dir = src_dir.canonicalize().context("source dir must exist")?;

        // Symlink user's dir into `src/` so mdbook watches the real files (live reload
        // works on save). We could also copy, but symlink is near-zero cost and lets
        // the user edit directly with their editor.
        // Note: mdbook's fs watcher follows symlinks.
        #[cfg(unix)]
        {
            let _ = std::fs::remove_dir(&book_src); // remove the empty dir we made
            std::os::unix::fs::symlink(&src_dir, &book_src)
                .context("failed to symlink source dir into workspace")?;
        }
        #[cfg(not(unix))]
        {
            copy_recursive(&src_dir, &book_src)?;
        }

        let cfg = BookConfig::new(&src_dir, title_override)?;
        write_book_toml(&book_root, &cfg)?;
        generate_summary(&book_src)?;
        install_theme(&book_root)?;

        Ok(Self { root: book_root, src: book_src })
    }
}

fn write_book_toml(root: &Path, cfg: &BookConfig) -> Result<()> {
    let tmpl_bytes = Assets::get("book-toml/book.toml.tmpl")
        .context("embedded book.toml.tmpl missing")?
        .data;
    let tmpl = std::str::from_utf8(tmpl_bytes.as_ref())?;
    let rendered = tmpl
        .replace("{{TITLE}}", &toml_escape(&cfg.title))
        .replace("{{AUTHOR}}", &toml_escape(&cfg.author))
        .replace("{{PLANTUML_SERVER}}", &toml_escape(&cfg.plantuml_server))
        .replace("{{SRC_DIR}}", &cfg.src_dir_display);
    std::fs::write(root.join("book.toml"), rendered)?;
    Ok(())
}

/// Minimal TOML string escaping — good enough for titles/paths (no control chars in our
/// inputs).
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Scan `src/` for `.md` files and write `SUMMARY.md` if one doesn't exist.
///
/// mdbook REQUIRES SUMMARY.md. If the user already has one we respect it; otherwise we
/// auto-generate from the directory structure (alphabetical, index.md first).
fn generate_summary(src: &Path) -> Result<()> {
    let summary = src.join("SUMMARY.md");
    if summary.exists() {
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = walk_md(src, src)?;
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
        out.push_str(&format!("- [{title}](./{})\n", rel.display()));
    }

    // SUMMARY lives at the root of src/ but because src is a symlink to the user's
    // dir, writing there would pollute. Use a sibling file under the book root.
    // However mdbook expects SUMMARY at src/. Solution: use create-missing=false and
    // place SUMMARY as a regular file in src_dir if writable, otherwise warn.
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

fn walk_md(dir: &Path, base: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // Skip hidden + common noise
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
            }
            out.extend(walk_md(&path, base)?);
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

    // mdbook-mermaid and mdbook-pagetoc need to install their own assets.
    // We run `mdbook-mermaid install .` from the workspace root — handled by serve.rs.
    // pagetoc.css/js copies are extracted via `mdbook-pagetoc install` IF its binary
    // supports install; otherwise we bundle a minimal fallback.
    // For now, we just make sure the theme dir exists; assets install happens in
    // serve.rs after we have a working book.toml.

    // Also create a dummy file if nothing in assets/ includes it — keeps rust_embed happy.
    let _ = extract_to; // suppress unused warning when copy_recursive fallback not used.

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
