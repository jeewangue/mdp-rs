use anyhow::Result;

use crate::theme::Assets;

pub fn run() -> Result<()> {
    for path in Assets::iter() {
        println!("===== {path} =====");
        if let Some(file) = Assets::get(&path) {
            match std::str::from_utf8(file.data.as_ref()) {
                Ok(s) => println!("{s}"),
                Err(_) => println!("<{} bytes binary>", file.data.len()),
            }
        }
    }
    Ok(())
}
