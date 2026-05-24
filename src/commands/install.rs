use anyhow::{Context, Result};
use std::process::Command;

const CRATES: &[(&str, &str)] = &[
    ("mdbook", "~0.5.2"),
    ("mdbook-katex", "=0.10.0-alpha"),
    ("mdbook-mermaid", "~0.17.0"),
    ("mdbook-plantuml", "~2.0.0"),
    ("mdbook-pagetoc", "~0.3.0"),
    ("mdbook-pandoc", "~0.11.0"),
];

const SYSTEM_BINS: &[(&str, &str)] = &[
    ("plantuml", "pacman -S plantuml / brew install plantuml"),
    ("mmdc", "npm i -g @mermaid-js/mermaid-cli"),
    ("pandoc", "pacman -S pandoc / brew install pandoc"),
    ("lualatex", "pacman -S texlive-luatex / brew install --cask mactex"),
];

pub fn run(force: bool) -> Result<()> {
    for (crate_name, version_req) in CRATES {
        if !force && which::which(crate_name).is_ok() {
            tracing::info!("skip {crate_name} (already installed — use --force to reinstall)");
            continue;
        }
        tracing::info!("cargo install {crate_name} --version {version_req}");
        let mut cmd = Command::new("cargo");
        cmd.arg("install")
            .arg(crate_name)
            .arg("--version")
            .arg(version_req)
            .arg("--locked");
        if force {
            cmd.arg("--force");
        }
        let status = cmd.status().context("failed to spawn cargo install")?;
        if !status.success() {
            anyhow::bail!("cargo install {crate_name} failed with {status}");
        }
    }
    tracing::info!("all required Rust preprocessors are installed");

    let mut missing_sys: Vec<(&str, &str)> = Vec::new();
    for &(bin, hint) in SYSTEM_BINS {
        if which::which(bin).is_ok() {
            tracing::info!("system dep {bin} ✓");
        } else {
            missing_sys.push((bin, hint));
        }
    }
    if !missing_sys.is_empty() {
        tracing::warn!("optional system dependencies missing (needed for diagram/PDF features):");
        for (bin, hint) in &missing_sys {
            tracing::warn!("  {bin}: {hint}");
        }
    }

    Ok(())
}
