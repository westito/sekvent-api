//! Records the target triple for `self-update` and makes the embedded
//! `templates/` and `skills/` trees trigger a rebuild when they change.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=SEKVENT_BUILD_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
    for path in [
        "../../templates",
        "../../skills",
        "../../Cargo.toml",
        "../../rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
}
