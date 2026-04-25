use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::Command;

use crate::preset::Workspace;

/// How many sequential ports to try after the requested one before giving up.
/// 16 covers "every nvim window in a typical session" with room to spare.
const PORT_FALLBACK_RANGE: u16 = 16;

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

    // Pick a port: prefer the requested one, but fall back to the next free
    // one if it's in use. This is what makes `:MdpOpen` from a second nvim
    // window work — the first claims 3456, the second auto-shifts to 3457.
    //
    // We bind+drop a TcpListener as a probe rather than calling `mdbook serve`
    // and parsing failure modes. There's a tiny TOCTOU window between probe
    // and mdbook actually binding, but for a local dev tool that's acceptable
    // (worst case the user retries `:MdpOpen`).
    let port = pick_free_port(host_ip, port).with_context(|| {
        format!(
            "no free port in [{port}, {})",
            port.saturating_add(PORT_FALLBACK_RANGE)
        )
    })?;

    let workspace = Workspace::prepare(&dir, title)?;
    tracing::info!("prepared workspace at {}", workspace.root.display());

    // Let mdbook-mermaid drop its JS files into the workspace.
    run_cmd(
        Command::new("mdbook-mermaid")
            .arg("install")
            .arg(&workspace.root),
        "mdbook-mermaid install",
    )?;

    // Print a stable, parseable line so the nvim plugin can find the URL even
    // when the port shifted from the requested default.
    println!("mdp: serving on http://{host}:{port}/");
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

/// Return a free port in the range `[requested, requested + PORT_FALLBACK_RANGE)`.
/// If the requested port itself is free, returns it unchanged. Otherwise warns
/// and returns the next available one. Errors only when the entire window is
/// taken.
fn pick_free_port(host: IpAddr, requested: u16) -> Result<u16> {
    let max = requested.saturating_add(PORT_FALLBACK_RANGE);
    for candidate in requested..max {
        if is_port_free(host, candidate) {
            if candidate != requested {
                tracing::warn!(
                    "port {requested} in use, using {candidate} instead"
                );
            }
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free port in [{requested}, {max})")
}

fn is_port_free(host: IpAddr, port: u16) -> bool {
    // bind + immediately drop: closes the listener but tells us the port is
    // available right now. SO_REUSEADDR is intentionally NOT set — if it were,
    // we could double-bind a port held in TIME_WAIT and confuse mdbook.
    TcpListener::bind(SocketAddr::new(host, port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn loopback() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    #[test]
    fn pick_free_port_returns_requested_when_free() {
        // Use a high random-ish port that's almost certainly free in CI.
        // OS will sometimes still take it; in that case the fallback should
        // pick a near neighbor — assert we get *something* in the window.
        let requested = 49_152; // start of dynamic/private port range
        let chosen = pick_free_port(loopback(), requested).unwrap();
        assert!((requested..requested + PORT_FALLBACK_RANGE).contains(&chosen));
    }

    #[test]
    fn pick_free_port_skips_busy_port() {
        // Bind one port, then ask for it. Should fall through to next free.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = listener.local_addr().unwrap().port();
        let chosen = pick_free_port(loopback(), busy).unwrap();
        assert_ne!(chosen, busy, "should have skipped the bound port");
        assert!(
            (busy..busy.saturating_add(PORT_FALLBACK_RANGE)).contains(&chosen),
            "chosen port {chosen} not in fallback window starting at {busy}"
        );
    }

    #[test]
    fn pick_free_port_window_constants_are_sane() {
        // Guard against `PORT_FALLBACK_RANGE = 0` — the function would
        // immediately bail without trying anything. We want at least 4
        // fallback slots.
        const _: () = assert!(PORT_FALLBACK_RANGE >= 4);
    }
}
