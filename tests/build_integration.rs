//! End-to-end integration tests: invoke the built `mdp` binary against a
//! temporary fixture directory, then parse the rendered HTML to assert that
//! the sidebar hierarchy and breadcrumb assets shipped correctly.
//!
//! These tests REQUIRE the mdbook preprocessors `mdp install-deps` would set
//! up (mdbook itself, mdbook-pagetoc, mdbook-katex, mdbook-mermaid,
//! mdbook-plantuml). When tools are missing we skip with a CLEAR message —
//! but if tools ARE present and `mdp build` fails, that's a real bug and we
//! panic. The previous `eprintln + return` pattern silently turned every
//! failure into a green test, which is exactly the class of false-pass that
//! lets a broken deploy artifact ship.

use std::path::Path;
use std::process::Command;

const REQUIRED_TOOLS: &[&str] = &[
    "mdbook",
    "mdbook-katex",
    "mdbook-mermaid",
    "mdbook-plantuml",
    "mdbook-pagetoc",
];

/// Returns Some(skip-message) when the env can't run the test, None when it
/// can. Centralises the skip decision so a missing preprocessor never masks
/// a real `mdp build` failure.
fn skip_reason() -> Option<String> {
    let missing: Vec<&str> = REQUIRED_TOOLS
        .iter()
        .filter(|bin| which::which(bin).is_err())
        .copied()
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "missing required tools ({}). run `mdp install-deps` to enable this test.",
            missing.join(", ")
        ))
    }
}

fn mdp_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set automatically by Cargo for the integration
    // test runner — points at the just-built binary.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mdp"))
}

fn write_fixture(root: &Path, layout: &[(&str, &str)]) {
    for (path, content) in layout {
        let p = root.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }
}

fn run_build(src: &Path, out: &Path) -> Result<String, String> {
    let output = Command::new(mdp_bin())
        .arg("build")
        .arg(src)
        .arg("-o")
        .arg(out)
        .output()
        .map_err(|e| format!("failed to spawn mdp: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(format!(
            "mdp build failed (exit={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        ));
    }
    Ok(format!("{stdout}\n{stderr}"))
}

#[test]
fn hierarchical_sidebar_renders_nested_sections() {
    let src = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    write_fixture(
        src.path(),
        &[
            ("index.md", "# Root Index"),
            ("top.md", "# Top Page"),
            ("subdir/sub.md", "# Sub Page"),
            ("subdir/nested/leaf.md", "# Nested Leaf"),
        ],
    );

    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    run_build(src.path(), out.path()).expect("mdp build must succeed when required tools are installed");

    let toc = std::fs::read_to_string(out.path().join("toc.html"))
        .expect("toc.html should be generated");

    // Top-level chapter list.
    assert!(toc.contains(r#"<ol class="chapter">"#), "no <ol class=\"chapter\">:\n{toc}");

    // Nested section MUST exist — proves hierarchy survived through to render.
    assert!(toc.contains(r#"<ol class="section">"#),
        "no nested <ol class=\"section\"> — sidebar still flat:\n{toc}");

    // Verify the ordering by looking for chapter numbers.
    // After the fix, "subdir" should be #3 (after Root Index and Top Page),
    // and the nested entries 3.1 and 3.2 should sit underneath.
    assert!(toc.contains("Root Index"), "Root Index missing");
    assert!(toc.contains("Top Page"), "Top Page missing");
    assert!(toc.contains(">subdir<") || toc.contains("> subdir<"),
        "draft chapter for subdir missing");
    assert!(toc.contains("Sub Page"), "Sub Page missing");
    assert!(toc.contains("Nested Leaf"), "Nested Leaf missing");
}

#[test]
fn breadcrumb_assets_are_bundled_and_referenced() {
    let src = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    write_fixture(
        src.path(),
        &[
            ("index.md", "# Root"),
            ("subdir/sub.md", "# Sub"),
        ],
    );

    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    run_build(src.path(), out.path()).expect("mdp build must succeed when required tools are installed");

    // The breadcrumb CSS + JS must end up in `theme/` (mdbook hashes the
    // filenames for cache-busting; we just want the prefix to exist).
    let theme = out.path().join("theme");
    let mut css_found = false;
    let mut js_found = false;
    for entry in std::fs::read_dir(&theme).unwrap() {
        let e = entry.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("mdp-breadcrumb") && name.ends_with(".css") {
            css_found = true;
        }
        if name.starts_with("mdp-breadcrumb") && name.ends_with(".js") {
            js_found = true;
        }
    }
    assert!(css_found, "mdp-breadcrumb.css not in theme dir: {:?}", std::fs::read_dir(&theme).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>());
    assert!(js_found, "mdp-breadcrumb.js not in theme dir");

    // The leaf page must reference both via additional-css/js.
    let leaf = std::fs::read_to_string(out.path().join("subdir/sub.html")).unwrap();
    assert!(
        leaf.contains("mdp-breadcrumb"),
        "leaf page missing breadcrumb asset reference"
    );
}

#[test]
fn user_supplied_summary_wins() {
    let src = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    write_fixture(
        src.path(),
        &[
            ("index.md", "# Root"),
            ("alpha.md", "# Alpha"),
            ("beta.md", "# Beta"),
            (
                "SUMMARY.md",
                "# Summary\n\n- [Custom Title](./index.md)\n- [Beta](./beta.md)\n",
            ),
        ],
    );

    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    run_build(src.path(), out.path()).expect("mdp build must succeed when required tools are installed");

    // Index page rendered the user's title; alpha.md is intentionally omitted
    // from the user's SUMMARY so it should NOT appear as a chapter.
    let toc = std::fs::read_to_string(out.path().join("toc.html")).unwrap();
    assert!(toc.contains("Custom Title"), "user title missing:\n{toc}");
    assert!(!toc.contains("Alpha"), "alpha.md should not be a chapter (user summary excluded it):\n{toc}");
}
