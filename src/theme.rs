//! Embedded theme + preprocessor assets.
//!
//! At build time, `rust-embed` pulls the `assets/` directory into the binary so the
//! compiled `mdp` runs without needing any external theme files on disk. When setting
//! up a workspace we expand these into the tmpdir.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
#[prefix = ""]
pub struct Assets;

