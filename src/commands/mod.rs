pub mod build;
pub mod dump;
pub mod install;
pub mod pdf;
pub mod preprocess;
pub mod serve;

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Replace stock `[preprocessor.plantuml]` with our `mdp preprocess` so
/// diagrams get the tokyonight skinparam header. Called by both `serve` and
/// `build` to keep their HTML output consistent.
pub fn register_mdp_preprocess(book_root: &std::path::Path) -> Result<()> {
    let book_toml = book_root.join("book.toml");
    let existing = std::fs::read_to_string(&book_toml).context("read book.toml")?;
    let filtered = serve::strip_preprocessor_blocks(&existing, &["plantuml"]);
    std::fs::write(&book_toml, filtered).context("rewrite book.toml")?;

    let self_exe = std::env::current_exe()
        .context("failed to resolve current mdp executable path")?;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&book_toml)
        .context("open book.toml for append")?;
    use std::io::Write as _;
    writeln!(
        f,
        "\n[preprocessor.mdp-diagrams]\ncommand = \"{} preprocess\"",
        crate::preset::toml_string_body_public(&self_exe.display().to_string())
    )?;
    Ok(())
}

/// Verify each named tool resolves on `PATH`. Bail with a friendly install
/// hint when any are missing — used by `serve`/`build` (mdbook stack) and
/// `pdf` (latex stack) to fail fast before mdbook's spawn errors leak out
/// as cryptic OS-level "No such file or directory".
pub fn ensure_tools(required: &[&str]) -> Result<()> {
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

const TAIL_LINES: usize = 64;

/// Watchdog config. Both timeouts default-from-env; pass 0 to disable a timer.
#[derive(Clone, Copy)]
pub struct Watchdog {
    pub overall: Duration,
    pub stall: Duration,
}

impl Watchdog {
    pub fn from_env(overall_default: u64, stall_default: u64) -> Self {
        fn read(name: &str, default: u64) -> Duration {
            Duration::from_secs(
                std::env::var(name)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(default),
            )
        }
        Self {
            overall: read("MDP_PDF_TIMEOUT", overall_default),
            stall: read("MDP_PDF_STALL_TIMEOUT", stall_default),
        }
    }
}

#[cfg(unix)]
fn kill_pgid(pid: i32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
    std::thread::sleep(Duration::from_secs(3));
    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_pgid(_pid: i32) {
    // Non-Unix: caller should use child.kill() instead.
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    is_stderr: bool,
    last_activity: Arc<AtomicU64>,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            let Ok(line) = line else { break };
            if is_stderr {
                let _ = writeln!(std::io::stderr(), "{line}");
            } else {
                let _ = writeln!(std::io::stdout(), "{line}");
            }
            last_activity.store(now_ms(), Ordering::SeqCst);
            let mut t = tail.lock().expect("tail mutex poisoned");
            if t.len() == TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
        }
    })
}

/// Run a child command with a stall + overall watchdog. On timeout, the
/// child's process group (Unix) is SIGTERM'd, then SIGKILL'd after a short
/// grace, and the last few output lines are surfaced in the error so the
/// caller can see what the wedged process was last doing.
///
/// Designed for `mdp pdf` where lualatex (deep down the mdbook → pandoc
/// chain) can wedge on SVG-heavy input or memory blowups with no clear
/// signal. Watchdog firing is the actionable diagnostic.
pub fn run_with_watchdog(mut cmd: Command, label: &str, wd: Watchdog) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;
    let pid = child.id() as i32;

    #[cfg(unix)]
    {
        // Move the child into its own process group so we can cascade-kill
        // the descendant tree (mdbook → pandoc → lualatex) via -pgid. Both
        // parent and child can race to call setpgid; whichever wins, the
        // second call is idempotent. Worst case: a Ctrl-C in the < 1ms
        // window before this call still reaches the child via the inherited
        // pgrp, which is a desirable outcome.
        let _ = nix::unistd::setpgid(
            nix::unistd::Pid::from_raw(pid),
            nix::unistd::Pid::from_raw(pid),
        );
    }

    // Forward Ctrl-C to the child group so an interactive interrupt actually
    // kills lualatex, not just leaves it orphaned. try_set_handler is a no-op
    // if a handler already exists (which it shouldn't, but harmless).
    #[cfg(unix)]
    {
        let pgid_for_signal = pid;
        let _ = ctrlc::try_set_handler(move || {
            kill_pgid(pgid_for_signal);
        });
    }

    let last_activity = Arc::new(AtomicU64::new(now_ms()));
    let tail: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
    let kill_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdout_thread = spawn_reader(stdout, false, last_activity.clone(), tail.clone());
    let stderr_thread = spawn_reader(stderr, true, last_activity.clone(), tail.clone());

    let start = Instant::now();
    let wd_done = done.clone();
    let wd_la = last_activity.clone();
    let wd_kill = kill_reason.clone();
    let watchdog = thread::spawn(move || {
        let stall_ms = wd.stall.as_millis() as u64;
        let overall_disabled = wd.overall.is_zero();
        loop {
            thread::sleep(Duration::from_millis(500));
            if wd_done.load(Ordering::SeqCst) {
                return;
            }
            if !overall_disabled && start.elapsed() > wd.overall {
                let mut k = wd_kill.lock().expect("kill_reason poisoned");
                if k.is_none() {
                    *k = Some(format!("exceeded overall timeout {}s", wd.overall.as_secs()));
                }
                kill_pgid(pid);
                return;
            }
            if stall_ms > 0 {
                let last = wd_la.load(Ordering::SeqCst);
                let elapsed_ms = now_ms().saturating_sub(last);
                if elapsed_ms > stall_ms {
                    let mut k = wd_kill.lock().expect("kill_reason poisoned");
                    if k.is_none() {
                        *k = Some(format!(
                            "no output for {}s (stall limit {}s) — likely lualatex hang",
                            elapsed_ms / 1000,
                            wd.stall.as_secs()
                        ));
                    }
                    kill_pgid(pid);
                    return;
                }
            }
        }
    });

    let status = child.wait().context("waiting for child")?;
    done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    if let Some(reason) = kill_reason.lock().expect("kill_reason poisoned").take() {
        let lines: Vec<String> = tail.lock().expect("tail poisoned").iter().cloned().collect();
        let body = if lines.is_empty() {
            "(no output captured)".to_string()
        } else {
            lines.join("\n")
        };
        anyhow::bail!(
            "{label} aborted by watchdog: {reason}\n\
             --- last {} output lines ---\n{body}\n--- end ---\n\
             tune via MDP_PDF_TIMEOUT / MDP_PDF_STALL_TIMEOUT (seconds; 0 to disable).",
            lines.len()
        );
    }

    if !status.success() {
        anyhow::bail!("{label} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    #[test]
    fn watchdog_passes_through_quick_command() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello && echo world");
        let wd = Watchdog {
            overall: Duration::from_secs(10),
            stall: Duration::from_secs(5),
        };
        run_with_watchdog(cmd, "echo-test", wd).expect("quick command should pass");
    }

    #[test]
    fn watchdog_kills_on_stall() {
        // No output for 3s; stall=1s → must trip stall watchdog.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 3; echo done");
        let wd = Watchdog {
            overall: Duration::from_secs(60),
            stall: Duration::from_secs(1),
        };
        let err = run_with_watchdog(cmd, "stall-test", wd).expect_err("should be killed by stall");
        let msg = format!("{err}");
        assert!(
            msg.contains("stall") || msg.contains("hang"),
            "expected stall message, got: {msg}"
        );
    }

    #[test]
    fn watchdog_kills_on_overall_timeout() {
        // Output every 100ms (no stall), but overall=2s while task takes 10s.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("for i in $(seq 1 100); do echo $i; sleep 0.1; done");
        let wd = Watchdog {
            overall: Duration::from_secs(2),
            stall: Duration::from_secs(60),
        };
        let err = run_with_watchdog(cmd, "overall-test", wd).expect_err("should be killed");
        let msg = format!("{err}");
        assert!(
            msg.contains("overall timeout"),
            "expected overall timeout message, got: {msg}"
        );
    }

    #[test]
    fn watchdog_zero_overall_disables_overall() {
        // Run for 2s with overall=0 (disabled), stall=5s → should pass.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("for i in $(seq 1 20); do echo $i; sleep 0.1; done");
        let wd = Watchdog {
            overall: Duration::ZERO,
            stall: Duration::from_secs(5),
        };
        run_with_watchdog(cmd, "no-overall", wd).expect("disabled overall should not trip");
    }
}
