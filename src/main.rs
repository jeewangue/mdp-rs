#![forbid(unsafe_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod commands;
mod config;
mod preset;
mod theme;

#[derive(Parser, Debug)]
#[command(
    name = "mdp",
    about = "Ad-hoc markdown previewer powered by mdbook",
    long_about = "Takes any directory of .md files and serves them in a browser with \
                  cross-file links, KaTeX, Mermaid, PlantUML and a heading-level sidebar TOC.\n\n\
                  Requires: mdbook, mdbook-pagetoc, mdbook-katex, mdbook-mermaid, mdbook-plantuml \
                  (run `mdp install-deps` to install all of them).",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Serve <dir> in a browser with live reload on file save.
    Serve {
        /// Directory of markdown files to preview.
        #[arg(default_value = ".")]
        dir: std::path::PathBuf,

        /// Port for the dev server.
        #[arg(short, long, default_value_t = 3456, env = "MDP_PORT")]
        port: u16,

        /// Host to bind.
        #[arg(short = 'H', long, default_value = "127.0.0.1", env = "MDP_HOST")]
        host: String,

        /// Open the default browser automatically.
        #[arg(long, default_value_t = false)]
        open: bool,

        /// Book title (defaults to directory name).
        #[arg(short, long)]
        title: Option<String>,
    },

    /// Build static HTML into <out>.
    Build {
        /// Source directory of markdown files.
        dir: std::path::PathBuf,

        /// Output directory.
        #[arg(short, long, default_value = "./book")]
        out: std::path::PathBuf,

        /// Book title.
        #[arg(short, long)]
        title: Option<String>,
    },

    /// Build a single PDF via mdbook-pandoc.
    Pdf {
        /// Source directory of markdown files.
        dir: std::path::PathBuf,

        /// Output PDF path.
        #[arg(short, long, default_value = "./book.pdf")]
        out: std::path::PathBuf,

        /// Book title.
        #[arg(short, long)]
        title: Option<String>,

        /// Pandoc output format (latex → PDF via LaTeX, html5 → self-contained HTML, etc).
        /// Default is latex; set to `html` for an HTML single-file.
        #[arg(long, default_value = "latex")]
        pandoc_to: String,
    },

    /// Install required mdbook preprocessors via cargo install.
    InstallDeps {
        /// Force reinstall even if already present.
        #[arg(long)]
        force: bool,
    },

    /// Print embedded book.toml template + theme files to stdout (debug).
    DumpAssets,

    /// mdbook preprocessor: render plantuml/mermaid fences to SVG at build time.
    /// Called automatically by `mdp pdf`. Reads JSON from stdin, writes JSON to
    /// stdout per the mdbook preprocessor protocol.
    ///
    /// Usage:
    ///   mdp preprocess                  # run transform (stdin → stdout)
    ///   mdp preprocess supports <name>  # capability check (exit 0 = supported)
    #[command(hide = true)]
    Preprocess {
        /// When present, checks capability for this renderer name and exits.
        supports: Option<String>,
        /// Renderer name when `supports` is the literal "supports" (mdbook calls
        /// `preprocess supports <renderer>`). Kept for protocol compatibility.
        renderer: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("MDP_LOG").unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { dir, port, host, open, title } => {
            commands::serve::run(dir, port, host, open, title)
        }
        Commands::Build { dir, out, title } => commands::build::run(dir, out, title),
        Commands::Pdf { dir, out, title, pandoc_to } => {
            commands::pdf::run(dir, out, title, pandoc_to)
        }
        Commands::InstallDeps { force } => commands::install::run(force),
        Commands::DumpAssets => commands::dump::run(),
        Commands::Preprocess { supports, renderer } => {
            commands::preprocess::run(supports, renderer)
        }
    }
}
