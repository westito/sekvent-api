//! Where a compiling command runs: here, or on the remote builder.
//!
//! The environment is passed in as a map so the decision is testable without
//! touching the process environment.

use std::collections::BTreeMap;

use crate::config::{RemoteConfig, RemoteMode};

/// A snapshot of environment variables.
pub type EnvMap = BTreeMap<String, String>;

/// Set by the remote builder image in every container it starts.
pub const RRB_CONTAINER_ENV: &str = "RRB_CONTAINER";

/// Set by CI systems.
pub const CI_ENV: &str = "CI";

/// `SEKVENT_LOCAL=1` (any value [`is_enabled`] accepts) forces local
/// execution.
pub const LOCAL_ENV: &str = "SEKVENT_LOCAL";

/// Path of the `rrb` remote builder front end on machines that have one.
/// With `[remote].mode = "auto"` its presence (non-empty) is what selects
/// remote execution.
pub const BUILD_CMD_ENV: &str = "BUILD_CMD";

/// Any non-empty value disables the "sekvent is behind" check.
pub const NO_UPDATE_CHECK_ENV: &str = "SEKVENT_NO_UPDATE_CHECK";

/// The process environment, skipping variables that are not UTF-8.
pub fn process_env() -> EnvMap {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// `key` is present with a non-empty value.
pub fn is_set(env: &EnvMap, key: &str) -> bool {
    env.get(key).is_some_and(|value| !value.is_empty())
}

/// `key` is set to a value that switches a flag on: anything except empty,
/// `0`, `false`, `no` or `off` (case-insensitive, surrounding whitespace
/// ignored). So `CI=false` or `SEKVENT_LOCAL=0` count as unset.
pub fn is_enabled(env: &EnvMap, key: &str) -> bool {
    env.get(key).is_some_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
    })
}

/// Where compiling commands run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// On this machine, for the given reason.
    Local(LocalReason),
    /// On the remote builder, through the given `rrb`.
    Remote(RrbSource),
}

/// Why a compiling command runs on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalReason {
    /// Already inside a remote builder container (`RRB_CONTAINER`).
    InsideBuilder,
    /// Running in CI (`CI`).
    Ci,
    /// `SEKVENT_LOCAL=1` (or another enabling value).
    ForcedByEnv,
    /// `[remote].mode = "local"`.
    ForcedByConfig,
    /// `[remote].mode = "auto"` and `BUILD_CMD` is not set.
    NoRemoteBuilder,
}

/// The `rrb` executable a remote location forwards through, as written
/// (a leading `~` is not expanded yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RrbSource {
    /// `[remote].rrb` in `sekvent.toml`.
    Config(String),
    /// The `BUILD_CMD` environment variable.
    Env(String),
    /// `[remote].mode = "rrb"` with neither `[remote].rrb` nor
    /// `BUILD_CMD`: forwarding fails with an error, never a local build.
    Missing,
}

/// `BUILD_CMD`, trimmed, when it is set to something non-blank.
fn remote_build(env: &EnvMap) -> Option<String> {
    env.get(BUILD_CMD_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Decide where compiling commands run.
///
/// In order: local inside the builder, in CI, with `SEKVENT_LOCAL=1` or
/// with `[remote].mode = "local"` (the three variables are read with
/// [`is_enabled`], so `CI=false` does not count as CI). Otherwise
/// `mode = "auto"` forwards through `BUILD_CMD` when it is set and runs
/// locally when it is not, and `mode = "rrb"` forwards through
/// `[remote].rrb`, else `BUILD_CMD`, else [`RrbSource::Missing`].
pub fn decide_location(env: &EnvMap, remote: &RemoteConfig) -> Location {
    if is_enabled(env, RRB_CONTAINER_ENV) {
        return Location::Local(LocalReason::InsideBuilder);
    }
    if is_enabled(env, CI_ENV) {
        return Location::Local(LocalReason::Ci);
    }
    if is_enabled(env, LOCAL_ENV) {
        return Location::Local(LocalReason::ForcedByEnv);
    }
    match remote.mode {
        RemoteMode::Local => Location::Local(LocalReason::ForcedByConfig),
        RemoteMode::Auto => remote_build(env)
            .map_or(Location::Local(LocalReason::NoRemoteBuilder), |rrb| {
                Location::Remote(RrbSource::Env(rrb))
            }),
        RemoteMode::Rrb => Location::Remote(match (&remote.rrb, remote_build(env)) {
            (Some(rrb), _) => RrbSource::Config(rrb.clone()),
            (None, Some(rrb)) => RrbSource::Env(rrb),
            (None, None) => RrbSource::Missing,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn config(mode: RemoteMode, rrb: Option<&str>) -> RemoteConfig {
        RemoteConfig {
            mode,
            rrb: rrb.map(str::to_owned),
        }
    }

    const BUILDER: (&str, &str) = ("BUILD_CMD", "/opt/rrb");

    fn from_env() -> Location {
        Location::Remote(RrbSource::Env("/opt/rrb".into()))
    }

    #[test]
    fn auto_is_the_default_and_follows_remote_build() {
        let auto = RemoteConfig::default();
        assert_eq!(auto.mode, RemoteMode::Auto);
        assert_eq!(
            decide_location(&env(&[]), &auto),
            Location::Local(LocalReason::NoRemoteBuilder)
        );
        for value in ["", "   "] {
            assert_eq!(
                decide_location(&env(&[("BUILD_CMD", value)]), &auto),
                Location::Local(LocalReason::NoRemoteBuilder),
                "{value:?}"
            );
        }
        assert_eq!(decide_location(&env(&[BUILDER]), &auto), from_env());
        assert_eq!(
            decide_location(&env(&[("BUILD_CMD", " ~/bin/rrb ")]), &auto),
            Location::Remote(RrbSource::Env("~/bin/rrb".into()))
        );
    }

    #[test]
    fn auto_ignores_a_configured_path() {
        let auto = config(RemoteMode::Auto, Some("/configured/rrb"));
        assert_eq!(
            decide_location(&env(&[]), &auto),
            Location::Local(LocalReason::NoRemoteBuilder)
        );
        assert_eq!(decide_location(&env(&[BUILDER]), &auto), from_env());
    }

    #[test]
    fn rrb_mode_prefers_the_configured_path_then_the_environment() {
        let pinned = config(RemoteMode::Rrb, Some("/configured/rrb"));
        let expected = Location::Remote(RrbSource::Config("/configured/rrb".into()));
        assert_eq!(decide_location(&env(&[]), &pinned), expected);
        assert_eq!(decide_location(&env(&[BUILDER]), &pinned), expected);

        let unpinned = config(RemoteMode::Rrb, None);
        assert_eq!(decide_location(&env(&[BUILDER]), &unpinned), from_env());
        assert_eq!(
            decide_location(&env(&[]), &unpinned),
            Location::Remote(RrbSource::Missing)
        );
        assert_eq!(
            decide_location(&env(&[("BUILD_CMD", "")]), &unpinned),
            Location::Remote(RrbSource::Missing)
        );
    }

    #[test]
    fn local_mode_never_forwards() {
        let local = config(RemoteMode::Local, Some("/configured/rrb"));
        for pairs in [&[][..], &[BUILDER][..]] {
            assert_eq!(
                decide_location(&env(pairs), &local),
                Location::Local(LocalReason::ForcedByConfig),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn each_signal_selects_local_in_every_mode() {
        for remote in [
            config(RemoteMode::Auto, None),
            config(RemoteMode::Rrb, Some("/configured/rrb")),
            config(RemoteMode::Rrb, None),
        ] {
            assert_eq!(
                decide_location(&env(&[("RRB_CONTAINER", "1"), BUILDER]), &remote),
                Location::Local(LocalReason::InsideBuilder)
            );
            assert_eq!(
                decide_location(&env(&[("CI", "true"), BUILDER]), &remote),
                Location::Local(LocalReason::Ci)
            );
            assert_eq!(
                decide_location(&env(&[("SEKVENT_LOCAL", "1"), BUILDER]), &remote),
                Location::Local(LocalReason::ForcedByEnv)
            );
        }
    }

    #[test]
    fn disabling_values_do_not_count() {
        let auto = RemoteConfig::default();
        for key in ["RRB_CONTAINER", "CI", "SEKVENT_LOCAL"] {
            for value in ["", "  ", "0", "false", "FALSE", "No", "off", " false "] {
                assert_eq!(
                    decide_location(&env(&[(key, value), BUILDER]), &auto),
                    from_env(),
                    "{key}={value:?}"
                );
            }
        }
        for value in ["yes", "true"] {
            assert_eq!(
                decide_location(&env(&[("SEKVENT_LOCAL", value), BUILDER]), &auto),
                Location::Local(LocalReason::ForcedByEnv)
            );
        }
        for pairs in [
            &[("CI", "false"), ("SEKVENT_LOCAL", "0"), BUILDER][..],
            &[("RRB_CONTAINER", "0"), ("CI", "no"), BUILDER][..],
        ] {
            assert_eq!(decide_location(&env(pairs), &auto), from_env(), "{pairs:?}");
        }
    }

    #[test]
    fn the_builder_wins_over_other_signals() {
        let pairs = [
            ("CI", "1"),
            ("RRB_CONTAINER", "1"),
            ("SEKVENT_LOCAL", "1"),
            BUILDER,
        ];
        assert_eq!(
            decide_location(&env(&pairs), &RemoteConfig::default()),
            Location::Local(LocalReason::InsideBuilder)
        );
    }
}
