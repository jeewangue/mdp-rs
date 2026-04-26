use anyhow::{Context, Result};
use std::process::Command;

/// (crate name, semver requirement). Pinning is critical — `mdbook` schema
/// changes (admonitions in 0.5+) and `mdbook-katex` 0.9 panicking against
/// mdbook 0.5.2 are not hypothetical. Tilde-pin for the rest so we accept
/// patch updates but block minor bumps that could break the template.
const CRATES: &[(&str, &str)] = &[
    ("mdbook", "~0.5.2"),
    // mdbook-katex 0.9.x panics against mdbook 0.5.2. Pin EXACTLY (`=`) — a
    // bare "0.10.0-alpha" is a SemVer requirement that matches any prerelease
    // with the same base (0.10.0-alpha.1, 0.10.0-alpha.2, …).
    ("mdbook-katex", "=0.10.0-alpha"),
    ("mdbook-mermaid", "~0.17.0"),
    ("mdbook-plantuml", "~2.0.0"),
    ("mdbook-pagetoc", "~0.3.0"),
    ("mdbook-pandoc", "~0.11.0"),
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
    tracing::info!("all required preprocessors are installed");
    Ok(())
}
