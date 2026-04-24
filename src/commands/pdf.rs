use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use crate::preset::Workspace;

pub fn run(
    dir: PathBuf,
    out: PathBuf,
    title: Option<String>,
    pandoc_to: String,
) -> Result<()> {
    ensure_deps()?;

    // Allow-list: pandoc-to is interpolated into a TOML value, so we reject
    // anything that isn't a recognized output format. (Without this, a payload
    // like `latex"\n[output.evil]` could inject arbitrary TOML keys.)
    const ALLOWED_PANDOC_TO: &[&str] = &[
        "latex", "html", "html5", "pdf", "docx", "epub", "epub3",
        "markdown", "markdown_strict", "markdown_github", "gfm",
        "plain", "rst", "commonmark", "commonmark_x",
    ];
    if !ALLOWED_PANDOC_TO.contains(&pandoc_to.as_str()) {
        anyhow::bail!(
            "unsupported --pandoc-to {pandoc_to:?}; allowed: {}",
            ALLOWED_PANDOC_TO.join(", ")
        );
    }

    let workspace = Workspace::prepare(&dir, title)?;
    let book_toml = workspace.root.join("book.toml");

    // Strip mermaid/plantuml preprocessor blocks — they transform fences into
    // HTML that pandoc can't render. Our `mdp-diagrams` preprocessor
    // (registered below) pre-renders them to SVG files instead.
    //
    // Also strip [output.html] — the PDF build path doesn't need HTML output,
    // and the template's `additional-js = ["mermaid.min.js", …]` would fail
    // since we stripped mermaid-install above.
    let existing = std::fs::read_to_string(&book_toml).context("read book.toml")?;
    let filtered = strip_preprocessor_blocks(&existing, &["mermaid", "plantuml"]);
    let filtered = strip_sections(&filtered, &["output.html", "output.html.fold", "output.html.search"]);
    std::fs::write(&book_toml, filtered).context("rewrite book.toml for pdf build")?;

    // Append the [output.pandoc] profile block to the generated book.toml so
    // mdbook-pandoc takes over as an additional renderer.
    let mut f = OpenOptions::new()
        .append(true)
        .open(&book_toml)
        .context("failed to open book.toml for append")?;

    // Use lualatex + Noto Sans CJK so Korean/CJK content renders instead of crashing
    // with "character not set up for use with LaTeX".
    let profile_block: String = if pandoc_to == "latex" {
        "\n[output.pandoc]\nhosted-html = \"\"\n\n\
         [output.pandoc.profile.pdf]\n\
         output-file = \"book.pdf\"\n\
         to = \"latex\"\n\
         pdf-engine = \"lualatex\"\n\
         \n\
         [output.pandoc.profile.pdf.variables]\n\
         mainfont = \"Noto Sans CJK KR\"\n\
         sansfont = \"Noto Sans CJK KR\"\n\
         monofont = \"Hack Nerd Font Mono\"\n\
         CJKmainfont = \"Noto Sans CJK KR\"\n\
         geometry = \"margin=1in\"\n\
         fontsize = \"10pt\"\n\
         colorlinks = true\n"
            .to_string()
    } else {
        // pandoc_to is allow-listed above, so interpolation is safe.
        format!(
            "\n[output.pandoc]\nhosted-html = \"\"\n\n\
             [output.pandoc.profile.pdf]\n\
             output-file = \"book.pdf\"\n\
             to = \"{pandoc_to}\"\n"
        )
    };
    writeln!(f, "{profile_block}")?;

    // Register our in-tree `mdp preprocess` preprocessor. It rewrites fenced
    // plantuml/mermaid blocks into pre-rendered SVG `![](path)` refs so pandoc
    // can include them in the PDF. mermaid/plantuml preprocessors are stripped
    // above, so there's no ordering conflict.
    let self_exe = std::env::current_exe()
        .context("failed to resolve current mdp executable path")?;
    writeln!(
        f,
        "\n[preprocessor.mdp-diagrams]\ncommand = \"{} preprocess\"",
        crate::preset::toml_string_body_public(&self_exe.display().to_string())
    )?;
    drop(f);


    // Run the build — this triggers both [output.html] and [output.pandoc] renderers.
    let status = Command::new("mdbook")
        .arg("build")
        .arg(&workspace.root)
        .status()
        .context("failed to spawn mdbook build")?;
    if !status.success() {
        anyhow::bail!("mdbook build failed: {status}");
    }

    // mdbook-pandoc writes output. Location depends on renderer count:
    //   single renderer (pandoc only)    → book/<profile>/book.pdf
    //   multiple renderers (html+pandoc) → book/<renderer>/<profile>/book.pdf
    // We stripped [output.html] above so we're in the single-renderer case,
    // but fall back to the multi-renderer path for robustness.
    let candidates = [
        workspace.root.join("book").join("pdf").join("book.pdf"),
        workspace.root.join("book").join("pandoc").join("pdf").join("book.pdf"),
    ];
    let produced = candidates
        .iter()
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "no PDF found after mdbook build. Tried: {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(produced, &out).with_context(|| {
        format!("failed to copy {} → {}", produced.display(), out.display())
    })?;

    tracing::info!("pdf written to {}", out.canonicalize().unwrap_or(out).display());
    Ok(())
}

/// Remove `[preprocessor.<name>]` sections (and their bodies + any dotted
/// subsections) from a book.toml. Returns the filtered string.
fn strip_preprocessor_blocks(toml: &str, names: &[&str]) -> String {
    let keys: Vec<String> = names.iter().map(|n| format!("preprocessor.{n}")).collect();
    strip_sections(toml, &keys.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Remove `[<key>]` sections from a TOML string. Also removes nested sections
/// (`[<key>.child]` etc.) — they wouldn't make sense without the parent.
///
/// Walks line-by-line; whenever a section header matches a strip key (exact
/// equality) OR begins with `<key>.` (nested), skip that whole section. Keep
/// scanning for the next non-stripped header and emit it.
fn strip_sections(toml: &str, keys: &[&str]) -> String {
    let matches = |header: &str| -> bool {
        keys.iter().any(|k| header == *k || header.starts_with(&format!("{k}.")))
    };

    let mut out = String::with_capacity(toml.len());
    let mut drop_current = false;

    for line in toml.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            // TOML section header — decide whether to start or stop dropping.
            let inner = &stripped[1..stripped.len() - 1];
            drop_current = matches(inner);
            if drop_current {
                continue;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if drop_current {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn ensure_deps() -> Result<()> {
    let required = ["mdbook", "mdbook-pandoc", "pandoc"];
    let missing: Vec<&str> = required
        .iter()
        .filter(|bin| which::which(bin).is_err())
        .copied()
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "missing tools: {}. install with: cargo install mdbook-pandoc (+ system pandoc)",
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod strip_tests {
    use super::strip_sections;

    #[test]
    fn strips_exact_section() {
        let input = "[book]\ntitle = 'x'\n\n[output.html]\nkey = 1\n\n[preprocessor.foo]\nk = 1\n";
        let out = strip_sections(input, &["preprocessor.foo"]);
        assert!(out.contains("[book]"));
        assert!(out.contains("[output.html]"));
        assert!(!out.contains("[preprocessor.foo]"));
        assert!(!out.contains("k = 1"));
    }

    #[test]
    fn strips_nested_sections() {
        let input =
            "[output.html]\ntheme = 'x'\n\n[output.html.fold]\nenable = true\n\n[output.html.search]\nlim = 1\n\n[preprocessor.katex]\nafter = []\n";
        let out = strip_sections(input, &["output.html"]);
        assert!(!out.contains("[output.html]"));
        assert!(!out.contains("[output.html.fold]"));
        assert!(!out.contains("[output.html.search]"));
        assert!(out.contains("[preprocessor.katex]"));
    }

    #[test]
    fn preserves_other_sections() {
        let input = "[preprocessor.pagetoc]\n\n[preprocessor.mermaid]\nfoo = 1\n\n[preprocessor.plantuml]\nbar = 2\n";
        let out = strip_sections(input, &["preprocessor.mermaid", "preprocessor.plantuml"]);
        assert!(out.contains("[preprocessor.pagetoc]"));
        assert!(!out.contains("[preprocessor.mermaid]"));
        assert!(!out.contains("[preprocessor.plantuml]"));
    }
}
