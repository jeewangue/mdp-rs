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
/// with image references. Blocks are matched at line granularity — fences must
/// start at column 0 and use exactly three backticks (typical GFM).
fn transform_markdown(src: &str, diagrams_dir: &Path) -> Result<String> {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let kind = if trimmed == "```plantuml" || trimmed == "```puml" {
            Some(DiagramKind::PlantUml)
        } else if trimmed == "```mermaid" {
            Some(DiagramKind::Mermaid)
        } else {
            None
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

#[derive(Copy, Clone)]
enum DiagramKind {
    PlantUml,
    Mermaid,
}

fn render_diagram(kind: DiagramKind, body: &str, diagrams_dir: &Path) -> Result<PathBuf> {
    let hash = blake3::hash(body.as_bytes()).to_hex();
    let name = match kind {
        DiagramKind::PlantUml => format!("plantuml-{}.svg", &hash[..16]),
        DiagramKind::Mermaid => format!("mermaid-{}.svg", &hash[..16]),
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
    // mmdc reads from file or stdin (`-i -`). It writes SVG when output ends in .svg.
    let mut child = Command::new("mmdc")
        .args([
            "--input", "-",
            "--output", out.to_str().context("non-utf8 output path")?,
            "--backgroundColor", "transparent",
            // `default` theme: dark text on transparent bg → readable on white PDF.
            "--theme", "default",
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
