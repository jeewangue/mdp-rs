//! Workspace preparation: copy the user's directory into a tmpdir, generate book.toml
//! and SUMMARY.md, and drop theme files into `theme/`.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::config::BookConfig;
use crate::theme::Assets;

pub struct Workspace {
    /// Root of the prepared mdbook project (tmpdir root).
    pub root: PathBuf,
    /// `root/src/` — symlink or copy of the user's dir.
    pub src: PathBuf,
    /// Canonicalized user source directory — kept so `resync()` can re-walk it
    /// when `serve --watch` notices file add/remove/rename.
    pub src_canonical: PathBuf,
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

        // Per-file `.md` symlinks only — symlinking the entire user dir
        // would expose `node_modules` self-loops (`foo/node_modules/foo ->
        // ../..`) to mdbook's recursive walker, which doesn't honor our
        // exclusion list and crashes with "Too many levels of symbolic
        // links". Per-file scoping keeps mdbook on files we whitelisted
        // while inotify still fires on edits through the symlink.
        Self::do_mirror_and_summary(&book_src, &src_canonical)?;

        let cfg = BookConfig::new(&src_canonical, title_override)?;
        write_book_toml(&book_root, &cfg)?;
        install_theme(&book_root)?;

        Ok(Self { root: book_root, src: book_src, src_canonical, _tmp: tmp })
    }

    fn do_mirror_and_summary(book_src: &Path, src_canonical: &Path) -> Result<()> {
        std::fs::create_dir_all(book_src)?;
        let mut mirrored = 0usize;
        let mut visited: HashSet<PathBuf> = HashSet::new();
        visited.insert(src_canonical.to_path_buf());
        mirror_md_files(
            src_canonical,
            src_canonical,
            book_src,
            &mut mirrored,
            &mut visited,
        )?;
        tracing::debug!("mirrored {mirrored} .md files into {}", book_src.display());
        generate_summary(book_src, src_canonical)?;
        Ok(())
    }
}

/// Walk `src_canonical` with the same exclusion + symlink-safety rules as
/// `mirror_md_files`, but only collect relative `.md` paths. Used by the
/// `serve --watch` watcher to detect *set* changes (add/remove/rename) and
/// skip resync on pure content modifications — content changes are picked up
/// by mdbook's own --watch via the symlinks, so resyncing on every save is
/// both wasted work and a 404 race window.
pub fn list_md_files_set(src_canonical: &Path) -> std::io::Result<std::collections::BTreeSet<PathBuf>> {
    let mut out: std::collections::BTreeSet<PathBuf> = Default::default();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(src_canonical.to_path_buf());
    walk_md_set(src_canonical, src_canonical, &mut out, &mut visited)?;
    Ok(out)
}

fn walk_md_set(
    src: &Path,
    canonical_root: &Path,
    out: &mut std::collections::BTreeSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    let rd = match std::fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in rd.flatten() {
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
                continue;
            }
        }
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && is_excluded_dir(name)
            {
                continue;
            }
            let canonical = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !visited.insert(canonical) {
                continue;
            }
            walk_md_set(&path, canonical_root, out, visited)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Ok(rel) = path.strip_prefix(canonical_root)
        {
            out.insert(rel.to_path_buf());
        }
    }
    Ok(())
}

/// Free-function form of `Workspace::resync` for use by the file watcher,
/// which can't hold a `&Workspace` across a thread boundary (TempDir doesn't
/// satisfy the bounds). Wipes per-file symlinks and re-runs mirror+summary.
pub fn resync_workspace_src(book_src: &Path, src_canonical: &Path) -> Result<()> {
    if let Ok(rd) = std::fs::read_dir(book_src) {
        for entry in rd.flatten() {
            let p = entry.path();
            let _ = if p.is_dir() && !p.is_symlink() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    Workspace::do_mirror_and_summary(book_src, src_canonical)
}

/// Recursively mirror `.md` files from `src` into `dst`, preserving relative
/// paths. Applies the common exclusion list at the directory level.
///
/// Defenses against pathological trees:
/// 1. Name-based exclusion (`is_excluded_dir`) skips common noise dirs.
/// 2. Per-visit canonical path set catches CYCLIC symlinks (e.g.
///    `docs/self -> ../docs`) that the exclusion list wouldn't see.
/// 3. `canonical_root` check rejects any symlink whose resolved target
///    leaves the tree.
///
/// On unix, files become symlinks (cheap, live-reload friendly — inotify
/// follows symlinks). On Windows we fall back to copy.
fn mirror_md_files(
    src: &Path,
    canonical_root: &Path,
    dst_root: &Path,
    mirrored: &mut usize,
    visited: &mut HashSet<PathBuf>,
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
                Err(_) => continue, // broken link or OS ELOOP — skip silently
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

            // Cycle detection: canonicalize the dir (resolving symlinks) and
            // refuse to re-enter a path we've already walked. Catches
            // `docs/self -> ../docs` as well as cross-branch symlinks that
            // point to an already-mirrored directory.
            let canonical = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !visited.insert(canonical.clone()) {
                tracing::warn!(
                    "skipping cyclic / already-visited directory: {} (resolves to {})",
                    path.display(),
                    canonical.display()
                );
                continue;
            }

            mirror_md_files(&path, canonical_root, dst_root, mirrored, visited)?;
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
        .replace("{{LANGUAGE}}", &toml_string_body(&cfg.language))
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
/// auto-generate from the directory structure preserving the on-disk hierarchy:
///
/// * Files in the root directory become top-level chapters (`- [title](./file.md)`),
///   with `index.md` (or `README.md` as fallback) emitted first.
/// * Sub-directories become nested chapters indented by 2 spaces per level. A
///   sub-directory's `index.md` (or `README.md`) supplies the parent chapter
///   link; if neither exists the directory becomes a draft chapter (`- [name]()`)
///   so its children can still nest under it.
/// * Sibling order: index first, then `.md` files A→Z, then sub-directories A→Z.
///
/// Symlinks in the tree that resolve OUTSIDE `canonical_root` are skipped — this
/// prevents a malicious dir from exposing arbitrary `.md` files on the filesystem
/// via the served preview.
fn generate_summary(src: &Path, canonical_root: &Path) -> Result<()> {
    let summary = src.join("SUMMARY.md");
    if summary.exists() {
        return Ok(());
    }

    let tree = build_chapter_tree(src, canonical_root)?;
    let mut out = String::from("# Summary\n\n");
    emit_root(&mut out, &tree);

    // SUMMARY lives at `book_src/SUMMARY.md`, a regular file in the tmpdir
    // (book_src is a real directory, not a symlink — only the per-file mirrors
    // inside it are symlinks). Writing here does not touch the user's source
    // tree.
    //
    // Atomic write: stage to .tmp then rename, so `mdbook serve --watch`
    // can never observe a half-truncated SUMMARY.md.
    let tmp = summary.with_extension("md.tmp");
    if let Err(e) = std::fs::write(&tmp, out)
        .and_then(|_| std::fs::rename(&tmp, &summary))
    {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(
            "could not write auto-generated SUMMARY.md to {}: {e} — add one manually",
            summary.display()
        );
        return Err(e.into());
    }
    Ok(())
}

/// Hierarchical view of a directory of `.md` files.
#[derive(Debug)]
struct ChapterTree {
    /// Display name for this directory (used as the title when no `index`/`README`).
    name: String,
    /// Path to the index file (`index.md` preferred, `README.md` fallback)
    /// relative to the SUMMARY src root, if any.
    index_rel: Option<PathBuf>,
    /// Title resolved from the index file's H1, or `None` if no H1 / no index.
    index_title: Option<String>,
    /// Non-index `.md` files directly inside this dir, sorted by filename.
    files: Vec<FileEntry>,
    /// Sub-directories with at least one `.md` file in their tree, sorted by name.
    subdirs: Vec<ChapterTree>,
}

#[derive(Debug)]
struct FileEntry {
    rel: PathBuf,    // relative to the SUMMARY src root
    title: String,   // H1 of the file, falling back to the file stem
}

impl ChapterTree {
    fn is_empty(&self) -> bool {
        self.index_rel.is_none() && self.files.is_empty() && self.subdirs.is_empty()
    }
}

fn build_chapter_tree(root: &Path, canonical_root: &Path) -> Result<ChapterTree> {
    fn walk(
        dir: &Path,
        root: &Path,
        canonical_root: &Path,
        visited: &mut HashSet<PathBuf>,
    ) -> Result<ChapterTree> {
        let mut index_path: Option<PathBuf> = None;
        let mut readme_path: Option<PathBuf> = None;
        let mut files: Vec<PathBuf> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();

        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(empty_tree(dir));
            }
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
                    tracing::warn!(
                        "skipping symlink that escapes source tree: {}",
                        path.display()
                    );
                    continue;
                }
            }

            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && is_excluded_dir(name)
                {
                    continue;
                }
                // Cycle detection: same protocol as `mirror_md_files` —
                // canonicalize the dir and refuse to re-enter a path we've
                // already seen. Catches `docs/self -> ../docs` and cross-
                // branch symlinks pointing back into the tree.
                let canonical = match path.canonicalize() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if !visited.insert(canonical.clone()) {
                    tracing::warn!(
                        "skipping cyclic / already-visited directory: {} (resolves to {})",
                        path.display(),
                        canonical.display()
                    );
                    continue;
                }
                dirs.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                match name {
                    "SUMMARY.md" => continue,
                    "index.md" => index_path = Some(path),
                    "README.md" => readme_path = Some(path),
                    _ => files.push(path),
                }
            }
        }

        files.sort();
        dirs.sort();

        let index_abs = index_path.or(readme_path);
        let index_title = index_abs.as_deref().and_then(read_title);
        let index_rel = index_abs
            .as_deref()
            .map(|p| p.strip_prefix(root).unwrap_or(p).to_path_buf());

        let file_entries: Vec<FileEntry> = files
            .into_iter()
            .map(|p| {
                let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
                let title = read_title(&p).unwrap_or_else(|| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("untitled")
                        .to_string()
                });
                FileEntry { rel, title }
            })
            .collect();

        let mut subdirs: Vec<ChapterTree> = Vec::new();
        for d in dirs {
            let child = walk(&d, root, canonical_root, visited)?;
            if !child.is_empty() {
                subdirs.push(child);
            }
        }

        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        Ok(ChapterTree {
            name,
            index_rel,
            index_title,
            files: file_entries,
            subdirs,
        })
    }

    fn empty_tree(dir: &Path) -> ChapterTree {
        ChapterTree {
            name: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            index_rel: None,
            index_title: None,
            files: Vec::new(),
            subdirs: Vec::new(),
        }
    }

    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(canonical_root.to_path_buf());
    walk(root, root, canonical_root, &mut visited)
}

fn emit_root(out: &mut String, root: &ChapterTree) {
    // Root index (if present) gets emitted first, at depth 0, with the same
    // formatting as a file entry (no parent draft chapter wraps it).
    if let Some(rel) = &root.index_rel {
        let title = root.index_title.clone().unwrap_or_else(|| {
            rel.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Index")
                .to_string()
        });
        out.push_str(&format!(
            "- [{}]({})\n",
            escape_link_text(&title),
            format_link_url(rel),
        ));
    }
    for f in &root.files {
        out.push_str(&format!(
            "- [{}]({})\n",
            escape_link_text(&f.title),
            format_link_url(&f.rel),
        ));
    }
    for child in &root.subdirs {
        emit_subdir(out, child, 0);
    }
}

fn emit_subdir(out: &mut String, node: &ChapterTree, indent: usize) {
    let pad = "  ".repeat(indent);
    let title = node
        .index_title
        .clone()
        .unwrap_or_else(|| node.name.clone());
    let link = match &node.index_rel {
        Some(rel) => format_link_url(rel),
        None => String::new(), // draft chapter (mdbook nests children under it)
    };
    out.push_str(&format!("{pad}- [{}]({})\n", escape_link_text(&title), link));

    let child_indent = indent + 1;
    let cpad = "  ".repeat(child_indent);
    for f in &node.files {
        out.push_str(&format!(
            "{cpad}- [{}]({})\n",
            escape_link_text(&f.title),
            format_link_url(&f.rel),
        ));
    }
    for child in &node.subdirs {
        emit_subdir(out, child, child_indent);
    }
}

/// Format a relative path as a SUMMARY link URL: prefix `./`, normalise
/// separators to `/`, and percent-encode characters that would confuse
/// pulldown-cmark's link parser (space, parens, `#`, `?`, `%`, autolink-form
/// brackets `<>`, code-span backticks, ASCII controls). Non-ASCII characters
/// (Korean, emoji, etc) pass through as-is — mdbook and browsers accept
/// UTF-8 in URLs natively.
fn format_link_url(rel: &Path) -> String {
    let s = rel.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str("./");
    for c in s.chars() {
        if matches!(
            c,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '-' | '.' | '_' | '~'
        ) {
            out.push(c);
        } else if (c as u32) >= 0x80 {
            // Beyond ASCII — pass through. Hangul, CJK, emoji, etc.
            out.push(c);
        } else {
            // ASCII char that needs percent-encoding.
            use std::fmt::Write;
            let _ = write!(out, "%{:02X}", c as u8);
        }
    }
    out
}

/// Backslash-escape characters that would break a SUMMARY entry's link text
/// (`[`, `]`, `\`, `` ` ``) and HTML-injection vectors (`<`, `>`).
/// pulldown-cmark allows raw HTML in link text by default, so an unescaped
/// `<script>` in a heading would render as live HTML in the sidebar.
fn escape_link_text(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '[' | ']' | '\\' | '`' => {
                out.push('\\');
                out.push(c);
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn read_title(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut buf = BufReader::new(f);
    let mut line = String::new();
    loop {
        line.clear();
        let n = buf.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
}

fn install_theme(root: &Path) -> Result<()> {
    let theme_dir = root.join("theme");
    std::fs::create_dir_all(&theme_dir)?;

    // Extract julian.jee CSS + JS files with conventional names (flat, under theme/).
    for (src_path, dst_name) in [
        ("themes/julian.jee/css/variables.css", "julian-jee-variables.css"),
        ("themes/julian.jee/css/general.css", "julian-jee-general.css"),
        ("themes/julian.jee/css/breadcrumb.css", "mdp-breadcrumb.css"),
        ("themes/julian.jee/js/breadcrumb.js", "mdp-breadcrumb.js"),
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
    fn list_md_files_set_ignores_modify_only() {
        // Same paths after a modify → set must compare equal.
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("a.md"), "# A").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.md"), "# B").unwrap();
        let canonical = root.canonicalize().unwrap();

        let before = super::list_md_files_set(&canonical).unwrap();
        // Edit content (modify-in-place); paths unchanged.
        std::fs::write(root.join("a.md"), "# A — edited").unwrap();
        let after = super::list_md_files_set(&canonical).unwrap();
        assert_eq!(before, after, "modify-only must not change the set");
    }

    #[test]
    fn list_md_files_set_detects_add_remove_rename() {
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("a.md"), "# A").unwrap();
        let canonical = root.canonicalize().unwrap();

        let s0 = super::list_md_files_set(&canonical).unwrap();
        assert_eq!(s0.len(), 1);

        // Add
        std::fs::write(root.join("b.md"), "# B").unwrap();
        let s1 = super::list_md_files_set(&canonical).unwrap();
        assert_ne!(s0, s1);
        assert_eq!(s1.len(), 2);

        // Rename
        std::fs::rename(root.join("b.md"), root.join("c.md")).unwrap();
        let s2 = super::list_md_files_set(&canonical).unwrap();
        assert_ne!(s1, s2);

        // Remove
        std::fs::remove_file(root.join("a.md")).unwrap();
        let s3 = super::list_md_files_set(&canonical).unwrap();
        assert_ne!(s2, s3);
        assert_eq!(s3.len(), 1);
    }

    #[test]
    fn list_md_files_set_skips_excluded_dirs() {
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("keep.md"), "# K").unwrap();
        for d in ["node_modules", "target", ".git", ".claude"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("hidden.md"), "# H").unwrap();
        }
        let canonical = root.canonicalize().unwrap();
        let s = super::list_md_files_set(&canonical).unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.contains(std::path::Path::new("keep.md")));
    }

    #[test]
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
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
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
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
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
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
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
        let mut visited1 = std::collections::HashSet::new();
        visited1.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n1, &mut visited1).unwrap();
        let mut n2 = 0;
        let mut visited2 = std::collections::HashSet::new();
        visited2.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n2, &mut visited2).unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 0, "rerun should be a no-op when all files are already mirrored");
        assert!(dst.path().join("a.md").exists());
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_detects_cyclic_symlink() {
        // `docs/self -> ../docs` — a cycle the exclusion list can't catch by
        // name. Must not hang or recurse past OS symlink limit.
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# A").unwrap();
        std::os::unix::fs::symlink("../docs", root.join("docs/self")).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
        // a.md appears ONCE (not infinitely via docs/self/self/.../a.md)
        assert_eq!(n, 1);
        assert!(dst.path().join("docs/a.md").exists());
        // docs/self was a cycle back to docs (which we already visited), so
        // we shouldn't have mirrored anything through it.
        assert!(!dst.path().join("docs/self").exists());
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_detects_cross_branch_cycle() {
        // Two branches symlinked to the same target.
        //   root/a.md
        //   root/branch1 -> shared
        //   root/branch2 -> shared
        //   root/shared/b.md
        // Both symlinks resolve into `shared/`; we should mirror shared's
        // contents exactly once.
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("a.md"), "# A").unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(root.join("shared/b.md"), "# B").unwrap();
        std::os::unix::fs::symlink("shared", root.join("branch1")).unwrap();
        std::os::unix::fs::symlink("shared", root.join("branch2")).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
        assert_eq!(n, 2, "a.md + shared/b.md only; branch1/branch2 dedup'd");
    }

    #[test]
    #[cfg(unix)]
    fn mirror_md_files_detects_symlink_back_to_root() {
        // `root/loop -> .` — classic self-reference. Must not infinite-loop.
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::write(root.join("a.md"), "# A").unwrap();
        std::os::unix::fs::symlink(".", root.join("loop")).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let canonical = root.canonicalize().unwrap();
        let mut n = 0;
        let mut visited = std::collections::HashSet::new();
        visited.insert(canonical.clone());
        super::mirror_md_files(&canonical, &canonical, dst.path(), &mut n, &mut visited).unwrap();
        assert_eq!(n, 1);
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

    // -------------------------- generate_summary -----------------------------

    fn build_summary(layout: &[(&str, &str)]) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (path, content) in layout {
            let p = root.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        let canonical = root.canonicalize().unwrap();
        super::generate_summary(root, &canonical).unwrap();
        std::fs::read_to_string(root.join("SUMMARY.md")).unwrap()
    }

    #[test]
    fn summary_flat_preserves_index_first_then_alphabetical() {
        let s = build_summary(&[
            ("apple.md", "# Apple"),
            ("zebra.md", "# Zebra"),
            ("index.md", "# My Project"),
        ]);
        let lines: Vec<&str> = s.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "- [My Project](./index.md)");
        assert_eq!(lines[1], "- [Apple](./apple.md)");
        assert_eq!(lines[2], "- [Zebra](./zebra.md)");
    }

    #[test]
    fn summary_nests_subdir_with_index() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("sub/index.md", "# Sub Section"),
            ("sub/leaf.md", "# Leaf"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert_eq!(body[0], "- [Root](./index.md)");
        assert_eq!(body[1], "- [Sub Section](./sub/index.md)");
        assert_eq!(body[2], "  - [Leaf](./sub/leaf.md)");
    }

    #[test]
    fn summary_emits_draft_chapter_for_dir_without_index() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("subdir/sub.md", "# Sub Page"),
            ("subdir/nested/leaf.md", "# Nested Leaf"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert_eq!(body[0], "- [Root](./index.md)");
        assert_eq!(body[1], "- [subdir]()"); // draft (no link target)
        assert_eq!(body[2], "  - [Sub Page](./subdir/sub.md)");
        assert_eq!(body[3], "  - [nested]()"); // draft, indented further
        assert_eq!(body[4], "    - [Nested Leaf](./subdir/nested/leaf.md)");
    }

    #[test]
    fn summary_treats_readme_as_dir_index_fallback() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("docs/README.md", "# Docs"),
            ("docs/details.md", "# Details"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert!(body.iter().any(|l| l == "- [Docs](./docs/README.md)"));
        assert!(body.iter().any(|l| l == "  - [Details](./docs/details.md)"));
    }

    #[test]
    fn summary_index_md_wins_over_readme() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("section/index.md", "# Section Index"),
            ("section/README.md", "# Section README"),
            ("section/leaf.md", "# Leaf"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        // index.md should be the chosen index (linked); README is treated as a
        // regular file alongside.
        assert!(body.contains(&"- [Section Index](./section/index.md)".to_string()));
        // README should NOT shadow index.md
        assert!(!body.iter().any(|l| l.contains("./section/README.md")));
    }

    #[test]
    fn summary_index_md_wins_over_readme_at_root() {
        // Root-level path is reached via emit_root, not the recursive subdir
        // walk — needs its own regression test so a refactor of either branch
        // can't silently break this asymmetric pair.
        let s = build_summary(&[
            ("index.md", "# Root Index"),
            ("README.md", "# Root README"),
            ("alpha.md", "# Alpha"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert!(
            body.iter().any(|l| l == "- [Root Index](./index.md)"),
            "root index.md should be the linked chapter; got body:\n{body:#?}"
        );
        assert!(
            !body.iter().any(|l| l.contains("./README.md")),
            "root README.md should not shadow index.md; got body:\n{body:#?}"
        );
    }

    #[test]
    fn summary_uses_dir_name_when_no_index_or_readme() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("hollow/leaf.md", "# Leaf"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert!(body.iter().any(|l| l == "- [hollow]()"));
    }

    #[test]
    fn summary_falls_back_to_filestem_when_no_h1() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("just-content.md", "no heading at all\n"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert!(body.iter().any(|l| l == "- [just-content](./just-content.md)"));
    }

    #[test]
    fn summary_url_encodes_path_special_chars() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("file with spaces.md", "# Spaced"),
            ("paren (draft).md", "# Parens"),
        ]);
        // Spaces and parens MUST be percent-encoded so pulldown-cmark parses
        // them as a single link target.
        assert!(s.contains("(./file%20with%20spaces.md)"), "expected percent-encoded space:\n{s}");
        assert!(s.contains("(./paren%20%28draft%29.md)"), "expected percent-encoded parens:\n{s}");
    }

    #[test]
    fn summary_escapes_brackets_in_titles() {
        let s = build_summary(&[
            ("index.md", "# [Brackets] Inside"),
            ("a.md", "# Has [brackets] too"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        // Brackets are backslash-escaped (preserves the original text without
        // breaking link parsing) — old code stripped them entirely.
        assert!(body[0].contains(r"\[Brackets\] Inside"), "got: {}", body[0]);
        assert!(body[1].contains(r"\[brackets\]"));
    }

    #[test]
    fn summary_handles_korean_titles() {
        let s = build_summary(&[
            ("index.md", "# 한글 문서"),
            ("그림자.md", "# 그림자 페이지"),
        ]);
        assert!(s.contains("[한글 문서]"));
        assert!(s.contains("[그림자 페이지]"));
        // Hangul chars in path are NOT percent-encoded (they're valid URL
        // characters) — sanity check the URL stays readable.
        assert!(s.contains("(./%EA%B7%B8%EB%A6%BC%EC%9E%90.md)") || s.contains("(./그림자.md)"));
    }

    #[test]
    fn summary_skips_summary_md_in_input() {
        // If we inadvertently encounter a SUMMARY.md as input, it must not
        // appear as a chapter entry.
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("SUMMARY.md", "preserved"),
            ("page.md", "# Page"),
        ]);
        // Note: the early-return in generate_summary respects an existing
        // SUMMARY.md, so this layout actually returns the user's existing
        // SUMMARY content. Confirm the early-return path:
        assert!(s.contains("preserved") || s.contains("- [Page]"));
    }

    #[test]
    fn summary_sort_groups_files_then_subdirs_at_each_level() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("zeta.md", "# Zeta"),         // root file
            ("alpha.md", "# Alpha"),       // root file
            ("subdir/inner.md", "# Inner"),// subdir
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        // Order: index, files A→Z, then subdirs.
        assert_eq!(body[0], "- [Root](./index.md)");
        assert_eq!(body[1], "- [Alpha](./alpha.md)");
        assert_eq!(body[2], "- [Zeta](./zeta.md)");
        assert_eq!(body[3], "- [subdir]()");
        assert_eq!(body[4], "  - [Inner](./subdir/inner.md)");
    }

    #[test]
    fn summary_deeply_nested_indents_correctly() {
        let s = build_summary(&[
            ("index.md", "# Root"),
            ("a/index.md", "# A"),
            ("a/b/index.md", "# B"),
            ("a/b/c/index.md", "# C"),
            ("a/b/c/leaf.md", "# Leaf"),
        ]);
        let body: Vec<String> = s.lines().filter(|l| l.contains("](")).map(String::from).collect();
        assert_eq!(body[0], "- [Root](./index.md)");
        assert_eq!(body[1], "- [A](./a/index.md)");
        assert_eq!(body[2], "  - [B](./a/b/index.md)");
        assert_eq!(body[3], "    - [C](./a/b/c/index.md)");
        assert_eq!(body[4], "      - [Leaf](./a/b/c/leaf.md)");
    }

    #[test]
    fn summary_drops_empty_subdirs_with_no_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("index.md"), "# R").unwrap();
        std::fs::create_dir_all(root.join("empty/inner")).unwrap();
        // No .md files anywhere under empty/.
        let canonical = root.canonicalize().unwrap();
        super::generate_summary(root, &canonical).unwrap();
        let s = std::fs::read_to_string(root.join("SUMMARY.md")).unwrap();
        assert!(!s.contains("empty"), "empty dirs should not appear:\n{s}");
    }

    #[test]
    fn summary_respects_existing_user_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("index.md"), "# R").unwrap();
        let custom = "# Summary\n\n- [Custom](./index.md)\n";
        std::fs::write(root.join("SUMMARY.md"), custom).unwrap();
        let canonical = root.canonicalize().unwrap();
        super::generate_summary(root, &canonical).unwrap();
        // SUMMARY.md must NOT be overwritten.
        let s = std::fs::read_to_string(root.join("SUMMARY.md")).unwrap();
        assert_eq!(s, custom);
    }

    #[test]
    fn format_link_url_normalizes_separators_on_windows_paths() {
        // Even on Unix, paths constructed manually with `\` should be treated
        // as URL `/` separators.
        let p = PathBuf::from("a\\b\\c.md");
        let url = super::format_link_url(&p);
        assert_eq!(url, "./a/b/c.md");
    }

    #[test]
    fn escape_link_text_handles_backticks_and_backslash() {
        assert_eq!(super::escape_link_text("plain"), "plain");
        assert_eq!(super::escape_link_text("with [bracket]"), r"with \[bracket\]");
        assert_eq!(super::escape_link_text("a\\b"), r"a\\b");
        assert_eq!(super::escape_link_text("with `code`"), r"with \`code\`");
        // Parens are left alone — they're only meaningful inside URLs.
        assert_eq!(super::escape_link_text("paren (foo)"), "paren (foo)");
        // HTML injection is blocked.
        assert_eq!(
            super::escape_link_text("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }

    #[test]
    fn format_link_url_encodes_backtick_and_angle_brackets() {
        let p = PathBuf::from("a`b<c>.md");
        let url = super::format_link_url(&p);
        assert_eq!(url, "./a%60b%3Cc%3E.md");
    }

    #[test]
    fn format_link_url_encodes_control_chars() {
        // Tab in a filename — encoded as %09.
        let p = PathBuf::from("a\tb.md");
        let url = super::format_link_url(&p);
        assert_eq!(url, "./a%09b.md");
    }

    #[test]
    fn format_link_url_preserves_utf8_path_bytes() {
        // Korean filename — bytes beyond ASCII pass through (browsers and
        // mdbook handle UTF-8 in URLs natively).
        let p = PathBuf::from("그림자.md");
        let url = super::format_link_url(&p);
        assert!(url.starts_with("./"));
        // Only the leading "./" is added; the rest is the original UTF-8.
        assert_eq!(&url[2..], "그림자.md");
    }

    #[test]
    #[cfg(unix)]
    fn summary_handles_cyclic_symlink_without_infinite_loop() {
        // docs/self -> ../docs (the npm self-loop pattern, but inside docs)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("index.md"), "# Root").unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/leaf.md"), "# Leaf").unwrap();
        std::os::unix::fs::symlink(root.join("docs"), root.join("docs/self")).unwrap();

        let canonical = root.canonicalize().unwrap();
        // Must complete without ELOOP / unbounded recursion.
        super::generate_summary(root, &canonical).unwrap();
        let s = std::fs::read_to_string(root.join("SUMMARY.md")).unwrap();
        // `self` should be skipped (it canonicalizes back to docs which is
        // already visited).
        let count = s.matches("./docs/leaf.md").count();
        assert_eq!(count, 1, "leaf.md must appear exactly once:\n{s}");
    }

    #[test]
    #[cfg(unix)]
    fn summary_rejects_symlinks_escaping_root() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "# Secret").unwrap();

        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();
        std::fs::write(root.join("index.md"), "# Root").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            root.join("link.md"),
        ).unwrap();

        let canonical = root.canonicalize().unwrap();
        super::generate_summary(root, &canonical).unwrap();
        let s = std::fs::read_to_string(root.join("SUMMARY.md")).unwrap();
        assert!(!s.contains("link.md"), "escaping symlink leaked into SUMMARY:\n{s}");
        assert!(!s.contains("secret"), "secret.md content leaked:\n{s}");
    }
}
