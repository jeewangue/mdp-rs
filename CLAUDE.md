# mdp-rs — agent instructions

`mdp` (markdown previewer) — Rust binary that takes any directory of
`.md` files and serves / builds / PDFs them via mdBook. README is the
user-facing reference; this file is the agent contract.

## Architecture

- **CLI dispatch** (`src/main.rs`, `src/commands/mod.rs`) — clap-based
  subcommands `serve`, `build`, `pdf`, `install-deps`. `ensure_tools`
  preflights every external dep and on miss prints a `mdp install-deps`
  hint.
- **Workspace generation** (`src/preset.rs`) — copies user's `.md` tree
  into a temp workspace, generates `book.toml` (with `language` resolved
  from `MDP_BOOK_LANG` / `$LANG` / "en"), generates `SUMMARY.md` from a
  recursive walk (cycle-detected, `index.md` / `README.md` fallback for
  per-dir landing pages).
- **Asset embedding** — `assets/` is embedded into the binary via
  `rust-embed`. `build.rs` emits `cargo:rerun-if-changed=assets/` so
  edits to themes/CSS/JS trigger a rebuild.
- **Live reload (`mdp serve`)** — `notify-debouncer-mini` watches the
  source dir. On `.md` add / rename / delete it regenerates SUMMARY;
  modify-only events do NOT resync (set-diff filter on the file list)
  because resyncing on every save creates a 404 race window between
  the regenerated SUMMARY and mdbook's HTML rebuild.
- **PDF (`mdp pdf`)** — runs `mdbook-pandoc` then `lualatex`. Wraps both
  in `run_with_watchdog` with stall + overall timers (`MDP_PDF_TIMEOUT`,
  `MDP_PDF_STALL_TIMEOUT`) and Unix `setpgid` for cascade kill.

## Stack

- Stable Rust, edition 2024. `rust-toolchain.toml` pins toolchain.
- mdbook (0.5+, native GFM alerts) + 4 preprocessors (`mdbook-katex`,
  `mdbook-mermaid`, `mdbook-plantuml`, `mdbook-pagetoc`), plus
  `mdbook-pandoc` for the PDF path.
- `notify-debouncer-mini` for filesystem watch.
- `tracing` for logging.
- `assert_cmd` + `tempfile` for integration tests.

## Build / test / run

```sh
make ci              # cargo build --release + cargo test --release + smoke
make smoke           # clean-room artifact gate (mktemp + run + assert)
cargo install --path . --locked
asdf reshim rust     # asdf users only
```

`scripts/smoke.sh` runs the release binary against a synthesised fixture
in a fresh `mktemp` and asserts on the rendered HTML — catches
asset-embedding regressions that unit tests can miss. Wire any new
asset path through `assets/` and verify smoke still passes.

## Code conventions

- `cargo clippy --all-targets -- -D warnings` must stay green.
- `forbid(unsafe_code)` at the crate level — do NOT add `unsafe` blocks
  unless you're prepared to lift the forbid (and explain why). Existing
  `setpgid` usage is from the parent process post-spawn, no `unsafe`
  needed.
- Errors: `thiserror` 2.x for library types, `anyhow` for CLI handlers.
- `tracing` everywhere. `info!`/`warn!`/`error!` for user-visible state,
  `debug!` for verbose diagnostic.
- Subprocess: `tokio::process::Command` only when async is needed,
  otherwise plain `std::process::Command` is fine for one-shot CLI
  invocations.

## Critical pitfalls

- **`mdp serve` modify-event resync is the 404-loop trap.**
  `notify-debouncer-mini`'s `DebouncedEventKind` is `Any | AnyContinuous`
  — it does NOT distinguish create / modify / delete. The fix is a
  set-diff: collect the current `.md` file set, compare to the previous
  set; only re-run SUMMARY generation if the set changed (add / rename
  / delete). Pure modifies are passed through to mdbook live reload as
  HTML rebuilds.
- **Never generate `SUMMARY.md` in the user's source dir.** It's an
  mdbook artifact and belongs in the workspace temp dir. The project
  root gitignores it as a guard against accidental writes leaking into
  the user's tree.
- **Don't drop `cargo:rerun-if-changed`.** `rust-embed` reads at compile
  time; without the rerun hint, edits to `assets/` silently no-op until
  you `cargo clean`.
- **lualatex hangs.** The watchdog timers (stall + overall) exist
  because lualatex can wedge silently on certain PDF inputs. Don't
  remove `run_with_watchdog`; if you bypass it for a one-off, set
  `MDP_KEEP_WORKSPACE=1` so the temp dir survives for inspection.
- **PlantUML local-binary execution is refused.** `MDP_PLANTUML_SERVER`
  must be an `http://` or `https://` URL. Path-style values are
  rejected at config parse to keep us off the local-codeexec path.

## Configuration env vars

| Var | Default | Effect |
| --- | ------- | ------ |
| `MDP_PORT` | `3456` | `serve --port` override. |
| `MDP_HOST` | `127.0.0.1` | Bind address. Non-loopback requires `MDP_ALLOW_NON_LOOPBACK`. |
| `MDP_ALLOW_NON_LOOPBACK` | unset | When `1`, lets `MDP_HOST` bind to a non-`127.x.x.x` address. |
| `MDP_BOOK_LANG` | `$LANG` first BCP-47 segment, fallback `en` | `<html lang>` value. |
| `MDP_AUTHOR` | `$USER` → `$USERNAME` → `mdp` | Author string in generated `book.toml`. |
| `MDP_PLANTUML_SERVER` | `https://www.plantuml.com/plantuml` | `http://` or `https://` URL. Path-style refused. |
| `MDP_PDF_TIMEOUT` | `600` | Overall PDF build timeout (s). `0` disables. |
| `MDP_PDF_STALL_TIMEOUT` | `60` | Kill PDF build after N seconds of no output. |
| `MDP_KEEP_WORKSPACE` | unset | Preserve temp workspace on PDF failure. |

## Testing patterns

- `tests/build_integration.rs` covers `mdp build` end-to-end.
  `skip_reason()` returns `Some` when a system dep is missing — tests
  use `.expect()` (not skip-on-fail) so missing deps are loud, not
  silent passes.
- Unit tests live alongside their modules (`#[cfg(test)] mod tests`).
- Integration smoke is `scripts/smoke.sh`. Update its expectations when
  shipping a visible HTML change.

## Roadmap (also in README)

- pagefind replacement for stock mdbook search.
- AUR package.
- xelatex / tectonic fallback when lualatex is missing.

## Remotes

`origin` → `git@gitlab.com:julian.jee/mdp-rs.git` (canonical)
`github` → `git@github.com:jeewangue/mdp-rs.git` (mirror)
