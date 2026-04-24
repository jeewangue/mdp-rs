use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::preset::Workspace;

pub fn run(
    dir: PathBuf,
    port: u16,
    host: String,
    open: bool,
    title: Option<String>,
) -> Result<()> {
    ensure_deps()?;

    let workspace = Workspace::prepare(&dir, title)?;
    tracing::info!("prepared workspace at {}", workspace.root.display());

    // Let mdbook-mermaid drop its JS files into the workspace.
    run_cmd(
        Command::new("mdbook-mermaid")
            .arg("install")
            .arg(&workspace.root),
        "mdbook-mermaid install",
    )?;

    // mdbook-pagetoc doesn't support `install` subcommand — copy its assets manually
    // from the extracted pagetoc crate's distribution (we ship a tiny fallback in
    // assets/themes/julian.jee/pagetoc/ if we need to). For now we rely on the user
    // having `mdbook-pagetoc`'s assets available; if missing we skip with a warning.
    // (The pagetoc preprocessor binary emits inline markup that doesn't strictly need
    // css/js to be present — the left sidebar still shows headings.)

    tracing::info!(
        "serving {} on http://{}:{}",
        workspace.src.display(),
        host,
        port
    );

    let mut cmd = Command::new("mdbook");
    cmd.arg("serve")
        .arg(&workspace.root)
        .arg("-p")
        .arg(port.to_string())
        .arg("-n")
        .arg(&host);
    if open {
        // We open the browser ourselves after a short delay so the user can see which
        // URL mdp is on, and we don't rely on mdbook's own --open semantics.
        let url = format!("http://{host}:{port}/");
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let _ = opener::open(&url);
        });
    }

    let status = cmd.status().context("failed to spawn `mdbook serve`")?;
    if !status.success() {
        anyhow::bail!("mdbook serve exited with {status}");
    }
    Ok(())
}

fn ensure_deps() -> Result<()> {
    let required = ["mdbook", "mdbook-katex", "mdbook-mermaid", "mdbook-plantuml", "mdbook-pagetoc"];
    let missing: Vec<&str> = required
        .iter()
        .filter(|bin| which::which(bin).is_err())
        .copied()
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "missing required tools: {}. run `mdp install-deps`",
            missing.join(", ")
        );
    }
    Ok(())
}

fn run_cmd(cmd: &mut Command, label: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("failed to spawn {label}"))?;
    if !status.success() {
        anyhow::bail!("{label} exited with {status}");
    }
    Ok(())
}
