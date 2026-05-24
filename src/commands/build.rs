use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::preset::Workspace;

const MDBOOK_TOOLS: &[&str] = &[
    "mdbook",
    "mdbook-katex",
    "mdbook-mermaid",
    "mdbook-plantuml",
    "mdbook-pagetoc",
];

pub fn run(dir: PathBuf, out: PathBuf, title: Option<String>) -> Result<()> {
    super::ensure_tools(MDBOOK_TOOLS)?;
    let workspace = Workspace::prepare(&dir, title)?;

    // mdbook-mermaid install
    let status = Command::new("mdbook-mermaid")
        .arg("install")
        .arg(&workspace.root)
        .status()
        .context("failed to spawn mdbook-mermaid install")?;
    if !status.success() {
        anyhow::bail!("mdbook-mermaid install failed: {status}");
    }

    super::register_mdp_preprocess(&workspace.root)?;

    let status = Command::new("mdbook")
        .arg("build")
        .arg(&workspace.root)
        .arg("-d")
        .arg(&out)
        .status()
        .context("failed to spawn mdbook build")?;
    if !status.success() {
        anyhow::bail!("mdbook build failed: {status}");
    }

    tracing::info!("static site written to {}", out.display());
    Ok(())
}
