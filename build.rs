// Tell Cargo to rebuild whenever any file under `assets/` changes. rust-embed
// reads `#[folder = "assets/"]` at compile time, but Cargo doesn't track the
// folder by default — without these hints, editing a CSS/JS theme asset
// silently no-ops `cargo build` until something else triggers a rebuild.

fn main() {
    println!("cargo:rerun-if-changed=assets");
    walk_and_emit(std::path::Path::new("assets"));
}

fn walk_and_emit(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // Emit cargo:rerun-if-changed for every file. Cargo de-dups and
        // handles deep trees fine; emitting per-file lets a file rename or
        // delete trigger the rebuild that a directory-only watch can miss
        // on some filesystems.
        if let Some(s) = p.to_str() {
            println!("cargo:rerun-if-changed={s}");
        }
        if p.is_dir() {
            walk_and_emit(&p);
        }
    }
}
