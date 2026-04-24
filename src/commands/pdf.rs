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

    let workspace = Workspace::prepare(&dir, title)?;

    // Append the [output.pandoc] profile block to the generated book.toml so
    // mdbook-pandoc takes over as an additional renderer.
    let book_toml = workspace.root.join("book.toml");
    let mut f = OpenOptions::new()
        .append(true)
        .open(&book_toml)
        .context("failed to open book.toml for append")?;
    // Use lualatex + Noto Sans CJK so Korean/CJK content renders instead of crashing
    // with "character not set up for use with LaTeX".
    let profile_block = if pandoc_to == "latex" {
        format!(
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
        )
    } else {
        format!(
            "\n[output.pandoc]\nhosted-html = \"\"\n\n\
             [output.pandoc.profile.pdf]\n\
             output-file = \"book.pdf\"\n\
             to = \"{pandoc_to}\"\n"
        )
    };
    writeln!(f, "{profile_block}")?;
    drop(f);

    // mdbook-mermaid install so Mermaid fences are rendered to SVG at build time
    // (mdbook-mermaid emits client JS for HTML; for PDF, Pandoc cannot run JS —
    // so Mermaid blocks appear as code. Future: switch to CLI-rendered SVG).
    let _ = Command::new("mdbook-mermaid")
        .arg("install")
        .arg(&workspace.root)
        .status();

    // Run the build — this triggers both [output.html] and [output.pandoc] renderers.
    let status = Command::new("mdbook")
        .arg("build")
        .arg(&workspace.root)
        .status()
        .context("failed to spawn mdbook build")?;
    if !status.success() {
        anyhow::bail!("mdbook build failed: {status}");
    }

    // mdbook-pandoc writes to book/pandoc/<profile>/output-file.
    // With multiple renderers mdbook puts each into book/<renderer>/<profile>/...
    let candidates = [
        workspace.root.join("book").join("pandoc").join("pdf").join("book.pdf"),
        workspace.root.join("book").join("pandoc/pdf/book.pdf"),
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

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::copy(produced, &out).with_context(|| {
        format!("failed to copy {} → {}", produced.display(), out.display())
    })?;

    tracing::info!("pdf written to {}", out.canonicalize().unwrap_or(out).display());
    Ok(())
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
