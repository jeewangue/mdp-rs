//! Runtime defaults, env-var overrides, and template placeholder resolution.

use anyhow::{Context, Result};
use std::path::Path;

pub struct BookConfig {
    pub title: String,
    pub author: String,
    pub plantuml_server: String,
    pub src_dir_display: String,
}

impl BookConfig {
    pub fn new(src_dir: &Path, title_override: Option<String>) -> Result<Self> {
        let title = title_override.unwrap_or_else(|| {
            src_dir
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "Markdown Preview".into())
        });

        let author =
            std::env::var("MDP_AUTHOR").unwrap_or_else(|_| whoami().unwrap_or_else(|| "mdp".into()));

        let plantuml_server = std::env::var("MDP_PLANTUML_SERVER")
            .unwrap_or_else(|_| "https://www.plantuml.com/plantuml".into());

        let src_dir_display = src_dir
            .canonicalize()
            .context("failed to canonicalize source directory")?
            .display()
            .to_string();

        Ok(Self { title, author, plantuml_server, src_dir_display })
    }
}

fn whoami() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}
