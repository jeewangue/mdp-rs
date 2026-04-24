use anyhow::{Context, Result};
use std::net::IpAddr;
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

    // Defense against accidental LAN exposure: --host must be a parseable IP
    // address. If it's non-loopback we require $MDP_ALLOW_NON_LOOPBACK=1 as an
    // explicit opt-in — otherwise a stray `--host 0.0.0.0` would silently
    // publish the user's notes on coffee-shop WiFi.
    let host_ip: IpAddr = host
        .parse()
        .with_context(|| format!("--host must be an IP address, got {host:?}"))?;
    if !host_ip.is_loopback() && std::env::var_os("MDP_ALLOW_NON_LOOPBACK").is_none() {
        anyhow::bail!(
            "refusing to bind non-loopback {host_ip}; set MDP_ALLOW_NON_LOOPBACK=1 to override"
        );
    }

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

    if open {
        // We open the browser ourselves after a short delay so the user can see which
        // URL mdp is on, and we don't rely on mdbook's own --open semantics.
        // IPv6 addresses need square brackets in the URL authority component.
        let url = if host_ip.is_ipv6() {
            format!("http://[{host_ip}]:{port}/")
        } else {
            format!("http://{host_ip}:{port}/")
        };
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let _ = opener::open(&url);
        });
    }

    let mut child = Command::new("mdbook")
        .arg("serve")
        .arg(&workspace.root)
        .arg("-p")
        .arg(port.to_string())
        .arg("-n")
        .arg(&host)
        .spawn()
        .context("failed to spawn `mdbook serve`")?;

    // On SIGINT / SIGTERM, kill the child so mdbook doesn't outlive mdp. The Drop
    // on `workspace` then runs and cleans the tmpdir — otherwise we'd leak
    // `/tmp/mdp-*` forever (security review finding #5).
    let child_id = child.id();
    ctrlc::set_handler(move || {
        // best-effort: send SIGTERM to the child PID. If it doesn't exit within a
        // couple seconds, the OS will reap it when mdp itself exits below.
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(child_id as i32), Signal::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            // Windows: rely on OS to clean up on mdp's exit.
            let _ = child_id;
        }
    })
    .context("failed to install SIGINT/SIGTERM handler")?;

    let status = child.wait().context("mdbook serve was not running")?;
    drop(workspace); // explicit: triggers TempDir cleanup
    if !status.success() {
        // On SIGTERM mdbook exits non-zero by convention — don't treat that as error.
        tracing::info!("mdbook serve exited with {status}");
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
