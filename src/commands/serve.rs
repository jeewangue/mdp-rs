use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::preset::{Workspace, list_md_files_set, resync_workspace_src};
use std::collections::BTreeSet;
use std::sync::Mutex;

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
    if !host_ip.is_loopback() && std::env::var("MDP_ALLOW_NON_LOOPBACK").as_deref() != Ok("1") {
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

    super::register_mdp_preprocess(&workspace.root)?;

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

    // Spawn a debounced filesystem watcher on the user's source directory. On
    // .md add/remove/rename, re-mirror + regenerate SUMMARY so mdbook --watch
    // picks up the change. Modifications to existing files are handled by
    // mdbook's own watcher (the symlinks point straight at the originals).
    let watch_handle = spawn_summary_watcher(workspace.src_canonical.clone(), &workspace);

    let status = child.wait().context("mdbook serve was not running")?;
    drop(watch_handle); // stop the watcher before tmpdir cleanup
    drop(workspace); // explicit: triggers TempDir cleanup
    if !status.success() {
        // On SIGTERM mdbook exits non-zero by convention — don't treat that as error.
        tracing::info!("mdbook serve exited with {status}");
    }
    Ok(())
}

/// Remove `[preprocessor.<name>]` blocks (and any nested subsections like
/// `[preprocessor.<name>.foo]`) from a book.toml. Shared with the pdf path's
/// stripper — kept inline here to avoid a circular module dep.
pub fn strip_preprocessor_blocks(toml: &str, names: &[&str]) -> String {
    let keys: Vec<String> = names.iter().map(|n| format!("preprocessor.{n}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let mut out = String::with_capacity(toml.len());
    let mut drop_current = false;
    for line in toml.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            let inner = &stripped[1..stripped.len() - 1];
            drop_current = key_refs
                .iter()
                .any(|k| inner == *k || inner.starts_with(&format!("{k}.")));
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

const MDBOOK_TOOLS: &[&str] = &[
    "mdbook",
    "mdbook-katex",
    "mdbook-mermaid",
    "mdbook-plantuml",
    "mdbook-pagetoc",
];

fn ensure_deps() -> Result<()> {
    super::ensure_tools(MDBOOK_TOOLS)
}

/// Handle returned by `spawn_summary_watcher`. Drop to stop the watcher.
struct WatcherHandle {
    stop: Arc<AtomicBool>,
    // Hold the debouncer so notify keeps watching for the watcher's lifetime.
    // notify_debouncer_mini::Debouncer drops cleanly on Drop.
    _debouncer: Option<Box<dyn std::any::Any + Send>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Watch `src_canonical` for filesystem events; on .md add/remove/rename
/// (NOT modify — mdbook --watch handles content edits via the symlinks),
/// re-mirror + regenerate SUMMARY.md so newly-added pages appear in the
/// sidebar without restarting `mdp serve`.
///
/// notify-debouncer-mini's `DebouncedEventKind` is `Any | AnyContinuous` —
/// it does NOT distinguish create from modify from delete. Dispatching on
/// kind alone would resync on every save, wiping the workspace symlinks for
/// the duration of the rebuild and causing mdbook --watch to render an empty
/// book mid-flight (= 404 in the browser when clicking a link). To avoid
/// that, we maintain a sorted set of relative `.md` paths and only resync
/// when the SET differs (add/remove/rename), not on content edits.
fn spawn_summary_watcher(src_canonical: PathBuf, ws: &Workspace) -> WatcherHandle {
    use notify_debouncer_mini::{notify::RecursiveMode, new_debouncer, DebouncedEvent};

    let book_src = ws.src.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_cb = stop.clone();
    let book_src_for_cb = book_src.clone();
    let src_for_cb = src_canonical.clone();

    // The known-set is seeded AFTER the watcher arms, so additions made in
    // the gap between snapshot and arm-time aren't silently swallowed (they
    // generate events that, with a pre-arm seed, would compare equal and
    // skip the resync). Until seeded, the closure compares against `None`
    // and treats any event as a real change.
    let known_set: Arc<Mutex<Option<BTreeSet<PathBuf>>>> = Arc::new(Mutex::new(None));
    let known_set_for_cb = known_set.clone();

    let debouncer = new_debouncer(
        Duration::from_millis(500),
        move |res: Result<Vec<DebouncedEvent>, _>| {
            if stop_for_cb.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = res {
                tracing::warn!("watch error: {e}");
                return;
            }
            let new_set = match list_md_files_set(&src_for_cb) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("source scan failed: {e}");
                    return;
                }
            };
            let mut known = known_set_for_cb.lock().expect("known_set poisoned");
            if known.as_ref() == Some(&new_set) {
                return; // content edit only — mdbook --watch handles it
            }
            if let Some(prev) = known.as_ref() {
                let added = new_set.difference(prev).count();
                let removed = prev.difference(&new_set).count();
                tracing::info!(
                    "source set changed (+{added} -{removed}) — resyncing SUMMARY"
                );
            } else {
                tracing::info!("first event before seed completed — resyncing SUMMARY");
            }
            if let Err(e) = resync_workspace_src(&book_src_for_cb, &src_for_cb) {
                tracing::warn!("resync failed: {e}");
                return;
            }
            *known = Some(new_set);
        },
    );

    let mut debouncer = match debouncer {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("file watcher unavailable: {e}; live SUMMARY regen disabled");
            return WatcherHandle { stop, _debouncer: None };
        }
    };

    if let Err(e) = debouncer.watcher().watch(&src_canonical, RecursiveMode::Recursive) {
        tracing::warn!("failed to watch {}: {e}", src_canonical.display());
        return WatcherHandle { stop, _debouncer: None };
    }

    *known_set.lock().expect("known_set poisoned") =
        Some(list_md_files_set(&src_canonical).unwrap_or_default());

    tracing::debug!("watching {} for SUMMARY regen", src_canonical.display());
    WatcherHandle { stop, _debouncer: Some(Box::new(debouncer)) }
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
