//! mdbook preprocessor — renders `plantuml` (and, for the pandoc renderer,
//! `mermaid`) fences to images that get embedded as `![alt](data:...)` data
//! URIs in the markdown.
//!
//! Why per-renderer behavior:
//! - **HTML serve**: replace plantuml fences ourselves so the tokyonight
//!   skinparam header is applied (`mdbook-plantuml` would otherwise call the
//!   public PlantUML server with no theme). Mermaid fences are LEFT ALONE so
//!   `mdbook-mermaid`'s client-side renderer keeps producing crisp vector SVG
//!   with theming via `themes/mermaid-config.json`.
//! - **PDF / pandoc**: replace BOTH plantuml and mermaid fences. Pandoc's
//!   LaTeX pipeline can't run JS for client-side mermaid, and mdbook-plantuml
//!   inline-svg output confuses pandoc's image embedding.
//!
//! Protocol reference: https://rust-lang.github.io/mdBook/for_developers/preprocessors.html
//!
//!   mdp preprocess supports <renderer>   # exit 0 if we transform for it
//!   mdp preprocess                       # read [ctx, book] JSON from stdin

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::theme::Assets;

const DIAGRAM_RENDER_TIMEOUT: Duration = Duration::from_secs(30);

fn wait_with_timeout(
    child: std::process::Child,
    timeout: Duration,
    label: &str,
) -> Result<std::process::Output> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(timeout)
        .map_err(|_| anyhow::anyhow!("{label}: timed out after {}s", timeout.as_secs()))?
        .context(format!("{label} I/O error"))
}

/// Which fence kinds should we transform for a given renderer?
#[derive(Copy, Clone, Debug)]
struct TransformPolicy {
    plantuml: bool,
    mermaid: bool,
}

impl TransformPolicy {
    fn for_renderer(renderer: &str) -> Self {
        match renderer {
            // HTML: only plantuml (mdbook-mermaid handles mermaid client-side).
            "html" => Self { plantuml: true, mermaid: false },
            // PDF: both — pandoc can't execute mermaid's client-side JS.
            "pandoc" => Self { plantuml: true, mermaid: true },
            // Unknown renderer — should not happen because mdbook only invokes
            // us for renderers we claimed in `supports`. Default to no-op.
            _ => Self { plantuml: false, mermaid: false },
        }
    }
}

/// Entry point. `supports_arg` is whatever mdbook passed as argv[1].
pub fn run(supports_arg: Option<String>, renderer: Option<String>) -> Result<()> {
    match supports_arg.as_deref() {
        Some("supports") => {
            let target = renderer.unwrap_or_default();
            if matches!(target.as_str(), "html" | "pandoc") {
                std::process::exit(0);
            }
            std::process::exit(1);
        }
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("read stdin")?;

            let input: Value = serde_json::from_str(&buf).context("parse preprocessor input")?;
            let context = input.get(0).cloned().unwrap_or(Value::Null);
            let mut book = input.get(1).cloned().unwrap_or(json!({}));

            let book_root = context
                .get("root")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .context("preprocessor context missing `root`")?;
            let renderer = context
                .get("renderer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let policy = TransformPolicy::for_renderer(&renderer);

            // Cache rendered images on disk so re-builds are fast. The dir
            // sits next to the book root, NOT inside the user's source tree.
            let cache_dir = book_root.join(".mdp-diagram-cache");
            std::fs::create_dir_all(&cache_dir)?;

            if let Some(items) = book.get_mut("items").and_then(|s| s.as_array_mut()) {
                for item in items.iter_mut() {
                    transform_section(item, &cache_dir, policy)?;
                }
            }

            let out = serde_json::to_string(&book).context("serialize book")?;
            std::io::stdout().write_all(out.as_bytes())?;
            Ok(())
        }
        Some(other) => {
            anyhow::bail!(
                "unknown preprocessor command: {other:?}. Expected `supports` or no args."
            );
        }
    }
}

fn transform_section(
    section: &mut Value,
    cache_dir: &Path,
    policy: TransformPolicy,
) -> Result<()> {
    if let Some(chapter) = section.get_mut("Chapter") {
        if let Some(content) = chapter.get_mut("content").and_then(|c| c.as_str()) {
            let transformed = transform_markdown(content, cache_dir, policy)?;
            chapter["content"] = Value::String(transformed);
        }
        if let Some(sub) = chapter.get_mut("sub_items").and_then(|s| s.as_array_mut()) {
            for item in sub.iter_mut() {
                transform_section(item, cache_dir, policy)?;
            }
        }
    }
    Ok(())
}

/// Find ```plantuml … ``` and ```mermaid … ``` fenced blocks and replace them
/// with image references. Tolerates:
/// - Leading whitespace on the fence (common in nested lists).
/// - Language identifier followed by more attributes (e.g. ```mermaid {theme=dark}).
/// - Both LF and CRLF line endings (str::lines handles both).
fn transform_markdown(src: &str, cache_dir: &Path, policy: TransformPolicy) -> Result<String> {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let kind_unfiltered = fence_kind(trimmed);
        // Apply the per-renderer policy: HTML only transforms plantuml so
        // mdbook-mermaid keeps owning mermaid blocks for client-side render.
        let kind = match kind_unfiltered {
            Some(DiagramKind::PlantUml) if policy.plantuml => Some(DiagramKind::PlantUml),
            Some(DiagramKind::Mermaid) if policy.mermaid => Some(DiagramKind::Mermaid),
            _ => None,
        };

        match kind {
            Some(k) => {
                let mut body = String::new();
                let mut closed = false;
                for inner in lines.by_ref() {
                    if inner.trim_start() == "```" {
                        closed = true;
                        break;
                    }
                    body.push_str(inner);
                    body.push('\n');
                }
                if !closed {
                    // Unterminated fence: emit opener + body + a synthetic
                    // closing ``` so the un-closed block doesn't swallow the
                    // rest of the document into a single fence. mdbook/pandoc
                    // still error on the malformed source, but the error is
                    // local to this block instead of cascading.
                    out.push_str(line);
                    out.push('\n');
                    out.push_str(&body);
                    out.push_str("```\n");
                    continue;
                }

                match render_diagram(k, &body, cache_dir) {
                    Ok((mime, bytes)) => {
                        // Embed as a data URI. Works in both HTML (browser-
                        // rendered <img>) and PDF (pandoc base64-decodes back
                        // to a temp file). Avoids "where do I put the SVG so
                        // both renderers can find it" pathing headaches.
                        let alt = match k {
                            DiagramKind::PlantUml => "plantuml diagram",
                            DiagramKind::Mermaid => "mermaid diagram",
                        };
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        out.push_str(&format!("![{alt}](data:{mime};base64,{b64})\n"));
                    }
                    Err(e) => {
                        eprintln!("[mdp preprocess] diagram render failed: {e}");
                        out.push_str(line);
                        out.push('\n');
                        out.push_str(&body);
                        out.push_str("```\n");
                        out.push_str(&format!(
                            "\n> diagram render error: {e}\n"
                        ));
                    }
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    Ok(out)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DiagramKind {
    PlantUml,
    Mermaid,
}

/// Match a fence opener line. Accepts:
///   ```plantuml           ← exact
///   ```puml               ← plantuml alias
///   ```mermaid            ← exact
///   ```plantuml {opts}    ← with trailing attributes (ignored)
///   ```mermaid:line-nums  ← colon-separated modifiers
///
/// Returns None for any other language (``` ```rust ``` etc) so we don't touch it.
fn fence_kind(line: &str) -> Option<DiagramKind> {
    let rest = line.strip_prefix("```")?;
    let lang = rest
        .split(|c: char| c.is_whitespace() || c == ':' || c == '{' || c == ',')
        .next()?;
    match lang {
        "plantuml" | "puml" => Some(DiagramKind::PlantUml),
        "mermaid" => Some(DiagramKind::Mermaid),
        _ => None,
    }
}

/// Render `body` to bytes, returning (MIME type, image bytes). Uses a
/// content-addressed file cache in `cache_dir` so a re-build with unchanged
/// fences is instant.
fn render_diagram(
    kind: DiagramKind,
    body: &str,
    cache_dir: &Path,
) -> Result<(&'static str, Vec<u8>)> {
    let hash = blake3::hash(body.as_bytes()).to_hex();
    let (mime, name) = match kind {
        // PlantUML → SVG. Plantuml outputs plain `<text>` elements, so it
        // embeds cleanly into both HTML and PDF without rasterisation.
        DiagramKind::PlantUml => (
            "image/svg+xml",
            format!("plantuml-{}.svg", &hash[..16]),
        ),
        // Mermaid v11 still emits `<foreignObject>` for labels by default and
        // there's no reliable `htmlLabels: false` switch for all diagram
        // types — pandoc's LaTeX pipeline drops the foreignObject text.
        // Rasterising to PNG ensures Korean / emoji / labels survive PDF
        // embedding.
        DiagramKind::Mermaid => ("image/png", format!("mermaid-{}.png", &hash[..16])),
    };
    let cache_path = cache_dir.join(name);

    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok((mime, bytes));
    }

    match kind {
        DiagramKind::PlantUml => render_plantuml(body, &cache_path)?,
        DiagramKind::Mermaid => render_mermaid(body, &cache_path)?,
    }
    let bytes = std::fs::read(&cache_path)
        .with_context(|| format!("read rendered diagram {}", cache_path.display()))?;
    Ok((mime, bytes))
}

fn render_plantuml(body: &str, out: &Path) -> Result<()> {
    // Prepend the tokyonight skinparam header so the diagram blends with the
    // julian.jee web theme.
    let header_bytes = Assets::get("themes/plantuml-tokyonight.puml")
        .context("embedded plantuml-tokyonight.puml missing")?
        .data;
    let header = std::str::from_utf8(header_bytes.as_ref())?;

    // PlantUML expects @startuml ... @enduml. If the user's body doesn't include
    // those, wrap it. We inject skinparams BETWEEN @startuml and the user's
    // content — they have to come inside the block.
    let composed = if body.contains("@startuml") {
        // Inject header right after the first @startuml line.
        let mut out_str = String::new();
        let mut injected = false;
        for line in body.lines() {
            out_str.push_str(line);
            out_str.push('\n');
            if !injected && line.trim_start().starts_with("@startuml") {
                out_str.push_str(header);
                injected = true;
            }
        }
        out_str
    } else {
        format!("@startuml\n{header}\n{body}\n@enduml\n")
    };

    let mut child = Command::new("plantuml")
        .args(["-pipe", "-tsvg", "-charset", "UTF-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `plantuml`. Install: pacman -S plantuml")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(composed.as_bytes())
        .context("write plantuml stdin")?;
    let output = wait_with_timeout(child, DIAGRAM_RENDER_TIMEOUT, "plantuml")?;
    if !output.status.success() {
        anyhow::bail!(
            "plantuml failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::write(out, &output.stdout)?;
    Ok(())
}


fn render_mermaid(body: &str, out: &Path) -> Result<()> {
    // Write the mermaid config alongside the output SVG so mmdc picks it up.
    // The config forces `htmlLabels: false` across diagram types — LaTeX /
    // rsvg-convert can't render `<foreignObject>`, so html labels silently drop
    // all Korean text. Plain `<text>` labels survive.
    let config_bytes = Assets::get("themes/mermaid-config.json")
        .context("embedded mermaid-config.json missing")?
        .data;
    let config_path = out.with_extension("config.json");
    std::fs::write(&config_path, config_bytes.as_ref())?;

    // mmdc reads from file or stdin (`-i -`). Output format is inferred from
    // the extension (.svg / .png / .pdf).
    let mut child = Command::new("mmdc")
        .args([
            "--input", "-",
            "--output", out.to_str().context("non-utf8 output path")?,
            "--backgroundColor", "white",
            "--configFile", config_path.to_str().context("non-utf8 config path")?,
            "--theme", "default",
            "--scale", "2",
            "--quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `mmdc`. Install: npm i -g @mermaid-js/mermaid-cli")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .context("write mmdc stdin")?;
    let output = wait_with_timeout(child, DIAGRAM_RENDER_TIMEOUT, "mmdc")?;
    if !output.status.success() {
        anyhow::bail!(
            "mmdc failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
#[cfg(test)]
mod fence_tests {
    use super::{DiagramKind, fence_kind};

    #[test]
    fn recognizes_basic_plantuml() {
        assert_eq!(fence_kind("```plantuml"), Some(DiagramKind::PlantUml));
        assert_eq!(fence_kind("```puml"), Some(DiagramKind::PlantUml));
    }

    #[test]
    fn recognizes_basic_mermaid() {
        assert_eq!(fence_kind("```mermaid"), Some(DiagramKind::Mermaid));
    }

    #[test]
    fn ignores_other_languages() {
        assert_eq!(fence_kind("```rust"), None);
        assert_eq!(fence_kind("```ts"), None);
        assert_eq!(fence_kind("```"), None);
        assert_eq!(fence_kind("```sh"), None);
    }

    #[test]
    fn accepts_attributes_after_language() {
        assert_eq!(fence_kind("```mermaid {theme=dark}"), Some(DiagramKind::Mermaid));
        assert_eq!(fence_kind("```mermaid:line-numbers"), Some(DiagramKind::Mermaid));
        assert_eq!(fence_kind("```plantuml , tag=foo"), Some(DiagramKind::PlantUml));
        assert_eq!(fence_kind("```mermaid [title=foo]"), Some(DiagramKind::Mermaid));
    }

    #[test]
    fn case_sensitive() {
        // Standard GFM fence languages are lowercase — our tests above assume
        // that. Uppercase is NOT treated as a match; if users want
        // flexibility, they can add an alias later.
        assert_eq!(fence_kind("```Mermaid"), None);
        assert_eq!(fence_kind("```PlantUML"), None);
    }

    #[test]
    fn rejects_non_fence_prefixes() {
        assert_eq!(fence_kind("``plantuml"), None); // only 2 backticks
        assert_eq!(fence_kind("````plantuml"), None); // 4 backticks (different fence style)
        assert_eq!(fence_kind("plantuml"), None);
        assert_eq!(fence_kind(""), None);
    }

    #[test]
    fn transform_leaves_non_diagram_code_alone() {
        let src = "# Hi\n\n```rust\nfn main() {}\n```\n\nBye\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path(), super::TransformPolicy { plantuml: true, mermaid: true }).unwrap();
        assert!(out.contains("```rust"));
        assert!(out.contains("fn main()"));
        assert!(out.contains("Bye"));
    }

    #[test]
    fn transform_unterminated_fence_passes_through() {
        // If a user has ```mermaid with no closing ``` we shouldn't swallow it.
        let src = "# Title\n\n```mermaid\ngraph TD\nA-->B\n\nno close here\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path(), super::TransformPolicy { plantuml: true, mermaid: true }).unwrap();
        assert!(out.contains("```mermaid"), "fence should be preserved on unterminated");
        assert!(out.contains("graph TD"));
    }

    #[test]
    fn transform_handles_crlf_line_endings() {
        // str::lines() strips both \n and \r\n, so CRLF input round-trips.
        let src = "# Title\r\n\r\n```rust\r\nfn a() {}\r\n```\r\n\r\nEnd.\r\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path(), super::TransformPolicy { plantuml: true, mermaid: true }).unwrap();
        assert!(out.contains("fn a()"));
        assert!(out.contains("End."));
    }

    #[test]
    fn transform_preserves_non_fence_content() {
        let src =
            "# Doc\n\nParagraph.\n\n```rust\nfn a() {}\n```\n\n| h | h |\n|---|---|\n| a | b |\n\n## Section\n\n- list\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path(), super::TransformPolicy { plantuml: true, mermaid: true }).unwrap();
        assert!(out.contains("Paragraph."));
        assert!(out.contains("```rust"));
        assert!(out.contains("| a | b |"));
        assert!(out.contains("## Section"));
    }

    #[test]
    fn fence_match_with_leading_indent_in_list() {
        // GFM allows fences inside list items with leading whitespace. The
        // caller does `trim_start` before calling fence_kind, so these match.
        assert_eq!(fence_kind("    ```mermaid".trim_start()), Some(DiagramKind::Mermaid));
        assert_eq!(fence_kind("  ```plantuml".trim_start()), Some(DiagramKind::PlantUml));
    }
}
