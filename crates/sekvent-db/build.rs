//! `sqlx::migrate!` embeds the test fixtures at compile time; stable Rust
//! does not track the files it reads, so a changed migration would leave a
//! stale test binary without this.

fn main() {
    println!("cargo:rerun-if-changed=tests/fixtures/migrations");
}
