use anyhow::{Context, Result};
use std::process::Command;

const CRATES: &[&str] = &[
    "mdbook",
    "mdbook-katex",
    "mdbook-mermaid",
    "mdbook-plantuml",
    "mdbook-pagetoc",
];

pub fn run(force: bool) -> Result<()> {
    for crate_name in CRATES {
        if !force && which::which(crate_name).is_ok() {
            tracing::info!("skip {crate_name} (already installed — use --force to reinstall)");
            continue;
        }
        tracing::info!("cargo install {crate_name}");
        let mut cmd = Command::new("cargo");
        cmd.arg("install").arg(crate_name).arg("--locked");
        if force {
            cmd.arg("--force");
        }
        // mdbook-katex 0.9.x panics against mdbook 0.5.2. Pin exactly with `=` — a
        // bare "0.10.0-alpha" is a SEMVER REQUIREMENT that matches any prerelease
        // with the same base version (0.10.0-alpha.1, 0.10.0-alpha.2, …). We want
        // an exact pin so a later prerelease can't be auto-pulled.
        if *crate_name == "mdbook-katex" {
            cmd.arg("--version").arg("=0.10.0-alpha");
        }
        let status = cmd.status().context("failed to spawn cargo install")?;
        if !status.success() {
            anyhow::bail!("cargo install {crate_name} failed with {status}");
        }
    }
    tracing::info!("all required preprocessors are installed");
    Ok(())
}
