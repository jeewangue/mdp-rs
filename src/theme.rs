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

/// Extract every asset into `dst`, preserving relative paths.
pub fn extract_to(dst: &std::path::Path) -> anyhow::Result<()> {
    for path in Assets::iter() {
        let file = Assets::get(&path).expect("embed path must resolve");
        let target = dst.join(path.as_ref());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, file.data.as_ref())?;
    }
    Ok(())
}
