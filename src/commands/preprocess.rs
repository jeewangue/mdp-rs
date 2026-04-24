//! mdbook preprocessor — renders `plantuml` and `mermaid` fences to SVG files
//! and replaces them with `![alt](path)` image refs. Only activates for the
//! `pandoc` renderer so the HTML path keeps using mdbook-mermaid / mdbook-plantuml
//! (which render client-side).
//!
//! Protocol reference: https://rust-lang.github.io/mdBook/for_developers/preprocessors.html
//!
//!   mdp preprocess supports <renderer>   # exit 0 if we transform for that renderer
//!   mdp preprocess                       # read [ctx, book] JSON from stdin,
//!                                        # write transformed book JSON to stdout

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::theme::Assets;

/// Entry point. `supports_arg` is whatever mdbook passed as argv[1].
pub fn run(supports_arg: Option<String>, renderer: Option<String>) -> Result<()> {
    match supports_arg.as_deref() {
        Some("supports") => {
            // Only claim support for pandoc. HTML renderer uses mdbook-mermaid /
            // mdbook-plantuml client-side; inserting pre-rendered SVGs there would
            // duplicate work and lose interactivity.
            let target = renderer.unwrap_or_default();
            if target == "pandoc" {
                std::process::exit(0);
            }
            std::process::exit(1);
        }
        None => {
            // Run the transform.
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("read stdin")?;

            let input: Value = serde_json::from_str(&buf).context("parse preprocessor input")?;
            // input is `[context, book]`.
            let context = input.get(0).cloned().unwrap_or(Value::Null);
            let mut book = input.get(1).cloned().unwrap_or(json!({}));

            // Directory where we spill SVGs. We put them inside the book root so
            // relative links from markdown → SVG survive pandoc's resolution.
            // `context.root` is the book root (dir containing book.toml).
            let book_root = context
                .get("root")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .context("preprocessor context missing `root`")?;
            let diagrams_dir = book_root.join("diagrams");
            std::fs::create_dir_all(&diagrams_dir)?;

            // Walk the book's section tree in place. mdbook's JSON uses
            // "items" at the top level (Book::items), not "sections".
            if let Some(items) = book.get_mut("items").and_then(|s| s.as_array_mut()) {
                for item in items.iter_mut() {
                    transform_section(item, &diagrams_dir)?;
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

fn transform_section(section: &mut Value, diagrams_dir: &Path) -> Result<()> {
    // mdbook's BookItem variants: {"Chapter": {...}}, {"Separator": null}, {"PartTitle": "..."}
    if let Some(chapter) = section.get_mut("Chapter") {
        if let Some(content) = chapter.get_mut("content").and_then(|c| c.as_str()) {
            let transformed = transform_markdown(content, diagrams_dir)?;
            chapter["content"] = Value::String(transformed);
        }
        if let Some(sub) = chapter.get_mut("sub_items").and_then(|s| s.as_array_mut()) {
            for item in sub.iter_mut() {
                transform_section(item, diagrams_dir)?;
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
fn transform_markdown(src: &str, diagrams_dir: &Path) -> Result<String> {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let kind = fence_kind(trimmed);

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
                    // unterminated fence — emit as-is so pandoc can complain
                    out.push_str(line);
                    out.push('\n');
                    out.push_str(&body);
                    continue;
                }

                match render_diagram(k, &body, diagrams_dir) {
                    Ok(svg_path) => {
                        // Use absolute path — pandoc's LaTeX pipeline resolves
                        // relative paths from each chapter's location, which we
                        // don't know here.
                        let alt = match k {
                            DiagramKind::PlantUml => "plantuml diagram",
                            DiagramKind::Mermaid => "mermaid diagram",
                        };
                        out.push_str(&format!(
                            "![{alt}]({})\n",
                            svg_path.display()
                        ));
                    }
                    Err(e) => {
                        // Rendering failed — emit original fence + a comment so
                        // the PDF shows SOMETHING rather than a crashing build.
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

fn render_diagram(kind: DiagramKind, body: &str, diagrams_dir: &Path) -> Result<PathBuf> {
    let hash = blake3::hash(body.as_bytes()).to_hex();
    let (ext, name) = match kind {
        // PlantUML → SVG (rsvg-convert handles plantuml's plain <text> elements fine).
        DiagramKind::PlantUml => ("svg", format!("plantuml-{}.svg", &hash[..16])),
        // Mermaid v11 still emits <foreignObject> with HTML labels by default and
        // there's no reliable `htmlLabels: false` for every diagram type. PNG
        // output rasterises the full browser-rendered diagram (text + fonts)
        // so pdflatex can include it. We lose vector sharpness but gain
        // guaranteed correctness for Korean / emoji / node labels.
        DiagramKind::Mermaid => ("png", format!("mermaid-{}.png", &hash[..16])),
    };
    let out = diagrams_dir.join(name);

    // Idempotent cache: if already rendered, reuse.
    if out.exists() {
        return Ok(out);
    }

    match kind {
        DiagramKind::PlantUml => render_plantuml(body, &out)?,
        DiagramKind::Mermaid => render_mermaid(body, &out)?,
    }
    let _ = ext;
    Ok(out)
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
    let output = child.wait_with_output()?;
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
            // `default` theme: dark text on white bg → readable on white PDF.
            "--theme", "default",
            // 2x scale keeps the rasterised PNG crisp when pdflatex embeds it.
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
    let output = child.wait_with_output()?;
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
        let out = super::transform_markdown(src, dst.path()).unwrap();
        assert!(out.contains("```rust"));
        assert!(out.contains("fn main()"));
        assert!(out.contains("Bye"));
    }

    #[test]
    fn transform_unterminated_fence_passes_through() {
        // If a user has ```mermaid with no closing ``` we shouldn't swallow it.
        let src = "# Title\n\n```mermaid\ngraph TD\nA-->B\n\nno close here\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path()).unwrap();
        assert!(out.contains("```mermaid"), "fence should be preserved on unterminated");
        assert!(out.contains("graph TD"));
    }

    #[test]
    fn transform_handles_crlf_line_endings() {
        // str::lines() strips both \n and \r\n, so CRLF input round-trips.
        let src = "# Title\r\n\r\n```rust\r\nfn a() {}\r\n```\r\n\r\nEnd.\r\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path()).unwrap();
        assert!(out.contains("fn a()"));
        assert!(out.contains("End."));
    }

    #[test]
    fn transform_preserves_non_fence_content() {
        let src =
            "# Doc\n\nParagraph.\n\n```rust\nfn a() {}\n```\n\n| h | h |\n|---|---|\n| a | b |\n\n## Section\n\n- list\n";
        let dst = tempfile::tempdir().unwrap();
        let out = super::transform_markdown(src, dst.path()).unwrap();
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
