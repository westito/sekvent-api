//! Files compiled into the binary from the sekvent repository: project
//! templates, agent skills, the root manifest (for dependency pins) and the
//! toolchain file.

use include_dir::{Dir, include_dir};

/// The repository's `templates/` tree; one top-level directory per kind.
pub static TEMPLATES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../templates");

/// The repository's `skills/` tree; one top-level directory per skill.
pub static SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../skills");

/// sekvent's root `Cargo.toml`, the source of the third-party pins.
pub const ROOT_CARGO_TOML: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"));

/// sekvent's `rust-toolchain.toml`.
pub const RUST_TOOLCHAIN_TOML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../rust-toolchain.toml"
));

/// The builder image projects use; its Rust version matches the toolchain.
pub const RRB_IMAGE: &str = "remote-builder:rust1.98.1-flutter3.47.2-r2";

/// Rust edition of generated projects.
pub const EDITION: &str = "2024";

/// The pinned toolchain channel from `rust-toolchain.toml`.
pub fn rust_version() -> String {
    toolchain_channel(RUST_TOOLCHAIN_TOML).unwrap_or_else(|| "stable".to_owned())
}

/// `[toolchain].channel` of a `rust-toolchain.toml`.
pub fn toolchain_channel(text: &str) -> Option<String> {
    let table: toml::Table = text.parse().ok()?;
    table
        .get("toolchain")?
        .get("channel")?
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toolchain_channel_is_read() {
        assert_eq!(
            rust_version(),
            toolchain_channel(RUST_TOOLCHAIN_TOML).unwrap()
        );
        assert!(RRB_IMAGE.contains(&format!("rust{}", rust_version())));
        assert_eq!(
            toolchain_channel("[toolchain]\nchannel = \"1.2.3\"\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(toolchain_channel("nope = 1"), None);
    }
}
