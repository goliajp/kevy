//! Minimal embedded usage: open an in-memory store, set + get a key.
//!
//! Run with: `cargo run -p kevy-embedded --example embedded`

use kevy_embedded::{Config, Store};

fn main() -> std::io::Result<()> {
    let s = Store::open(Config::default())?;
    s.set(b"greeting", b"hello, kevy")?;
    let v = s.get(b"greeting")?;
    println!("greeting = {}", String::from_utf8_lossy(v.as_deref().unwrap_or(b"<missing>")));
    println!("dbsize = {}", s.dbsize());
    Ok(())
}
