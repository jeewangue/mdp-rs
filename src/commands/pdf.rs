use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use crate::preset::Workspace;

pub fn run(
    dir: PathBuf,
    out: PathBuf,
    title: Option<String>,
    pandoc_to: String,
) -> Result<()> {
    ensure_deps()?;

    // Allow-list: pandoc-to is interpolated into a TOML value, so we reject
    // anything that isn't a recognized output format. (Without this, a payload
    // like `latex"\n[output.evil]` could inject arbitrary TOML keys.)
    const ALLOWED_PANDOC_TO: &[&str] = &[
        "latex", "html", "html5", "pdf", "docx", "epub", "epub3",
        "markdown", "markdown_strict", "markdown_github", "gfm",
        "plain", "rst", "commonmark", "commonmark_x",
    ];
    if !ALLOWED_PANDOC_TO.contains(&pandoc_to.as_str()) {
        anyhow::bail!(
            "unsupported --pandoc-to {pandoc_to:?}; allowed: {}",
            ALLOWED_PANDOC_TO.join(", ")
        );
    }

    ensure_latex_engine(&pandoc_to)?;

    let workspace = Workspace::prepare(&dir, title)?;
    let book_toml = workspace.root.join("book.toml");
    tracing::info!("pdf workspace at {}", workspace.root.display());

    // Strip mermaid/plantuml preprocessor blocks — they transform fences into
    // HTML that pandoc can't render. Our `mdp-diagrams` preprocessor
    // (registered below) pre-renders them to SVG files instead.
    //
    // Also strip [output.html] — the PDF build path doesn't need HTML output,
    // and the template's `additional-js = ["mermaid.min.js", …]` would fail
    // since we stripped mermaid-install above.
    let existing = std::fs::read_to_string(&book_toml).context("read book.toml")?;
    let filtered = strip_preprocessor_blocks(&existing, &["mermaid", "plantuml"]);
    let filtered = strip_sections(&filtered, &["output.html", "output.html.fold", "output.html.search"]);
    std::fs::write(&book_toml, filtered).context("rewrite book.toml for pdf build")?;

    // Append the [output.pandoc] profile block to the generated book.toml so
    // mdbook-pandoc takes over as an additional renderer.
    let mut f = OpenOptions::new()
        .append(true)
        .open(&book_toml)
        .context("failed to open book.toml for append")?;

    // Use lualatex + Noto Sans CJK so Korean/CJK content renders instead of crashing
    // with "character not set up for use with LaTeX".
    let profile_block: String = if pandoc_to == "latex" {
        // LaTeX profile:
        //   - lualatex: the only engine that renders color emoji and handles
        //     Korean + arbitrary Unicode well.
        //   - mainfont / CJKmainfont: Noto Sans CJK KR covers both Korean and
        //     ASCII glyphs. Hack Nerd Font for code keeps nerdfont glyphs.
        //   - header-includes is DELIBERATELY MINIMAL: adding fvextra or
        //     redefining Highlighting blows up lualatex memory (multi-GB) on
        //     realistic docs. Pandoc's defaults already wrap code lines via
        //     `\usepackage[breaksymbolleft=..., breaklines]{fvextra}` — we
        //     just ask pandoc to set the `highlighting-macros` variable so the
        //     right package gets loaded.
        //   - Color emoji rendering via Noto Color Emoji restricted to the
        //     emoji/symbol Unicode ranges so the harfbuzz fallback only fires
        //     for the specific codepoints that need it.
        "\n[output.pandoc]\nhosted-html = \"\"\n\n\
         [output.pandoc.profile.pdf]\n\
         output-file = \"book.pdf\"\n\
         to = \"latex\"\n\
         pdf-engine = \"lualatex\"\n\
         \n\
         [output.pandoc.profile.pdf.variables]\n\
         monofont = \"Hack Nerd Font Mono\"\n\
         monofontoptions = \"Scale=0.85\"\n\
         CJKmainfont = \"Noto Sans CJK KR\"\n\
         geometry = \"margin=1in\"\n\
         fontsize = \"10pt\"\n\
         colorlinks = true\n\
         linkcolor = \"Navy\"\n\
         urlcolor = \"Navy\"\n\
         toccolor = \"Navy\"\n\
         header-includes = ['''\n\
         % Emoji fallback — Noto Color Emoji via HarfBuzz. Restricted to emoji\n\
         % / dingbat / pictograph Unicode ranges so lualatex doesn't query the\n\
         % emoji font for every missed glyph (cheap for typical prose; slow if\n\
         % applied to code blocks).\n\
         %   2600-27BF   misc symbols + dingbats\n\
         %   1F000-1FFFF emoji planes\n\
         %   1F100-1F1FF regional indicators (flag letters)\n\
         %\n\
         % The main/sans font is set via \\AtEndPreamble so it runs AFTER\n\
         % pandoc's template (which would otherwise overwrite the fallback).\n\
         % Monospace is NOT given the fallback — HarfBuzz fallback on Verbatim\n\
         % content is pathologically slow on large docs.\n\
         \\usepackage{fontspec}\n\
         \\directlua{luaotfload.add_fallback(\"emojifallback\", {\"NotoColorEmoji:mode=harf;script=dflt;ranges=1F000-1FFFF,2600-27BF,1F100-1F1FF\"})}\n\
         \\AtEndPreamble{\n\
           \\setmainfont{Noto Sans CJK KR}[RawFeature={fallback=emojifallback}]\n\
           \\setsansfont{Noto Sans CJK KR}[RawFeature={fallback=emojifallback}]\n\
         }\n\
         \\tracinglostchars=0\n\
         ''']\n"
            .to_string()
    } else {
        // pandoc_to is allow-listed above, so interpolation is safe.
        format!(
            "\n[output.pandoc]\nhosted-html = \"\"\n\n\
             [output.pandoc.profile.pdf]\n\
             output-file = \"book.pdf\"\n\
             to = \"{pandoc_to}\"\n"
        )
    };
    writeln!(f, "{profile_block}")?;

    // Register our in-tree `mdp preprocess` preprocessor. It rewrites fenced
    // plantuml/mermaid blocks into pre-rendered SVG `![](path)` refs so pandoc
    // can include them in the PDF. mermaid/plantuml preprocessors are stripped
    // above, so there's no ordering conflict.
    let self_exe = std::env::current_exe()
        .context("failed to resolve current mdp executable path")?;
    writeln!(
        f,
        "\n[preprocessor.mdp-diagrams]\ncommand = \"{} preprocess\"",
        crate::preset::toml_string_body_public(&self_exe.display().to_string())
    )?;
    drop(f);


    // Run the build — this triggers both [output.html] and [output.pandoc] renderers.
    // Watchdog wraps the spawn: lualatex deep down the chain is the most common
    // wedge point on SVG-heavy or memory-blowup docs. Stall-detection
    // (no-output-for-N-seconds) is the strongest signal the underlying TeX
    // engine is stuck, since healthy lualatex prints page-by-page progress.
    let mut cmd = Command::new("mdbook");
    cmd.arg("build").arg(&workspace.root);
    // Defaults: 600s overall (LaTeX builds CAN legitimately take ~5min on
    // larger docs); 60s stall — lualatex prints at least one line per page.
    let wd = super::Watchdog::from_env(600, 60);
    let result = super::run_with_watchdog(cmd, "mdbook build (pdf)", wd);
    if let Err(e) = result {
        if std::env::var_os("MDP_KEEP_WORKSPACE").is_some() {
            // Leak the TempDir so the user can inspect book.toml + generated .tex / logs.
            let root = workspace.root.clone();
            std::mem::forget(workspace);
            tracing::error!(
                "preserved workspace at {} (MDP_KEEP_WORKSPACE set)",
                root.display()
            );
        }
        return Err(e);
    }

    // mdbook-pandoc writes output. Location depends on renderer count:
    //   single renderer (pandoc only)    → book/<profile>/book.pdf
    //   multiple renderers (html+pandoc) → book/<renderer>/<profile>/book.pdf
    // We stripped [output.html] above so we're in the single-renderer case,
    // but fall back to the multi-renderer path for robustness.
    let candidates = [
        workspace.root.join("book").join("pdf").join("book.pdf"),
        workspace.root.join("book").join("pandoc").join("pdf").join("book.pdf"),
    ];
    let produced = candidates
        .iter()
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "no PDF found after mdbook build. Tried: {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(produced, &out).with_context(|| {
        format!("failed to copy {} → {}", produced.display(), out.display())
    })?;

    if std::env::var_os("MDP_KEEP_WORKSPACE").is_some() {
        let root = workspace.root.clone();
        std::mem::forget(workspace);
        tracing::info!("preserved workspace at {} (MDP_KEEP_WORKSPACE set)", root.display());
    }

    tracing::info!("pdf written to {}", out.canonicalize().unwrap_or(out).display());
    Ok(())
}

/// Remove `[preprocessor.<name>]` sections (and their bodies + any dotted
/// subsections) from a book.toml. Returns the filtered string.
fn strip_preprocessor_blocks(toml: &str, names: &[&str]) -> String {
    let keys: Vec<String> = names.iter().map(|n| format!("preprocessor.{n}")).collect();
    strip_sections(toml, &keys.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Remove `[<key>]` sections from a TOML string. Also removes nested sections
/// (`[<key>.child]` etc.) — they wouldn't make sense without the parent.
///
/// Walks line-by-line; whenever a section header matches a strip key (exact
/// equality) OR begins with `<key>.` (nested), skip that whole section. Keep
/// scanning for the next non-stripped header and emit it.
fn strip_sections(toml: &str, keys: &[&str]) -> String {
    let matches = |header: &str| -> bool {
        keys.iter().any(|k| header == *k || header.starts_with(&format!("{k}.")))
    };

    let mut out = String::with_capacity(toml.len());
    let mut drop_current = false;

    for line in toml.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            // TOML section header — decide whether to start or stop dropping.
            let inner = &stripped[1..stripped.len() - 1];
            drop_current = matches(inner);
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

fn ensure_deps() -> Result<()> {
    let required = ["mdbook", "mdbook-pandoc", "pandoc"];
    let missing: Vec<&str> = required
        .iter()
        .filter(|bin| which::which(bin).is_err())
        .copied()
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "missing tools: {}. install with: cargo install mdbook-pandoc (+ system pandoc)",
            missing.join(", ")
        );
    }
    Ok(())
}

/// `pandoc-to=latex` (the default profile) shells out to lualatex; without
/// it pandoc surfaces a cryptic mid-build error long after the user thinks
/// the build is succeeding. Pre-flight here so the failure is a clear "your
/// system is missing X" message before any work starts.
fn ensure_latex_engine(pandoc_to: &str) -> Result<()> {
    if pandoc_to != "latex" {
        return Ok(());
    }
    if which::which("lualatex").is_err() {
        anyhow::bail!(
            "missing lualatex (required for --pandoc-to=latex). install texlive-luatex \
             (Arch: `pacman -S texlive-luatex texlive-langkorean`; Debian/Ubuntu: \
             `apt install texlive-luatex texlive-lang-korean`) or pass \
             --pandoc-to=html for an HTML-only build."
        );
    }
    Ok(())
}

#[cfg(test)]
mod strip_tests {
    use super::strip_sections;

    #[test]
    fn strips_exact_section() {
        let input = "[book]\ntitle = 'x'\n\n[output.html]\nkey = 1\n\n[preprocessor.foo]\nk = 1\n";
        let out = strip_sections(input, &["preprocessor.foo"]);
        assert!(out.contains("[book]"));
        assert!(out.contains("[output.html]"));
        assert!(!out.contains("[preprocessor.foo]"));
        assert!(!out.contains("k = 1"));
    }

    #[test]
    fn strips_nested_sections() {
        let input =
            "[output.html]\ntheme = 'x'\n\n[output.html.fold]\nenable = true\n\n[output.html.search]\nlim = 1\n\n[preprocessor.katex]\nafter = []\n";
        let out = strip_sections(input, &["output.html"]);
        assert!(!out.contains("[output.html]"));
        assert!(!out.contains("[output.html.fold]"));
        assert!(!out.contains("[output.html.search]"));
        assert!(out.contains("[preprocessor.katex]"));
    }

    #[test]
    fn preserves_other_sections() {
        let input = "[preprocessor.pagetoc]\n\n[preprocessor.mermaid]\nfoo = 1\n\n[preprocessor.plantuml]\nbar = 2\n";
        let out = strip_sections(input, &["preprocessor.mermaid", "preprocessor.plantuml"]);
        assert!(out.contains("[preprocessor.pagetoc]"));
        assert!(!out.contains("[preprocessor.mermaid]"));
        assert!(!out.contains("[preprocessor.plantuml]"));
    }

    #[test]
    fn strip_is_idempotent() {
        let input = "[output.html]\nx = 1\n\n[book]\ntitle = 't'\n";
        let first = super::strip_sections(input, &["output.html"]);
        let second = super::strip_sections(&first, &["output.html"]);
        assert_eq!(first, second);
        assert!(!first.contains("[output.html]"));
        assert!(first.contains("[book]"));
    }

    #[test]
    fn strip_handles_section_with_whitespace_prefix() {
        // Sub-table definitions sometimes have leading whitespace when the
        // file is hand-formatted. Our matcher should still find them.
        let input = "  [preprocessor.foo]\nkey = 1\n[book]\n";
        let out = super::strip_sections(input, &["preprocessor.foo"]);
        assert!(!out.contains("[preprocessor.foo]"));
        assert!(out.contains("[book]"));
    }

    #[test]
    fn strip_drops_everything_until_next_header() {
        // By design: comments/blank lines between a stripped header and the
        // next header are part of the stripped section and go with it.
        // Callers that want to preserve comments should put them AFTER the
        // next section header.
        let input = "[drop]\na = 1\n\n# comment-belongs-to-drop\n[keep]\nb = 2\n# comment-belongs-to-keep\n";
        let out = super::strip_sections(input, &["drop"]);
        assert!(!out.contains("[drop]"));
        assert!(!out.contains("comment-belongs-to-drop"));
        assert!(out.contains("[keep]"));
        assert!(out.contains("b = 2"));
        assert!(out.contains("comment-belongs-to-keep"));
    }

    #[test]
    fn strip_empty_input_is_empty() {
        assert_eq!(super::strip_sections("", &["anything"]), "");
    }

    #[test]
    fn strip_no_match_returns_unchanged_content() {
        let input = "[a]\nx=1\n\n[b]\ny=2\n";
        let out = super::strip_sections(input, &["c"]);
        // `strip_sections` normalizes trailing newline — compare semantically.
        assert!(out.contains("[a]"));
        assert!(out.contains("x=1"));
        assert!(out.contains("[b]"));
        assert!(out.contains("y=2"));
    }

    #[test]
    fn strip_preprocessor_blocks_maps_names_correctly() {
        let input = "[preprocessor.mermaid]\nx=1\n[preprocessor.plantumlextra]\ny=2\n";
        // We should NOT strip `preprocessor.plantumlextra` even though it
        // starts with "plantuml" — strict equality for the top-level section
        // name (dot match is whole-segment).
        let out = super::strip_preprocessor_blocks(input, &["mermaid", "plantuml"]);
        assert!(!out.contains("[preprocessor.mermaid]"));
        assert!(out.contains("[preprocessor.plantumlextra]"));
    }
}
