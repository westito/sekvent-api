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

/// `SEKVENT_LOCAL=1` forces local execution.
pub const LOCAL_ENV: &str = "SEKVENT_LOCAL";

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

/// Where compiling commands run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// On this machine, for the given reason.
    Local(LocalReason),
    /// On the remote builder, through `rrb`.
    Remote,
}

/// Why a compiling command runs on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalReason {
    /// Already inside a remote builder container (`RRB_CONTAINER`).
    InsideBuilder,
    /// Running in CI (`CI`).
    Ci,
    /// `SEKVENT_LOCAL=1`.
    ForcedByEnv,
    /// `[remote].mode = "local"`.
    ForcedByConfig,
}

/// Decide where compiling commands run.
///
/// Local when inside the builder, in CI, with `SEKVENT_LOCAL=1` or with
/// `[remote].mode = "local"`; remote otherwise.
pub fn decide_location(env: &EnvMap, remote: &RemoteConfig) -> Location {
    if is_set(env, RRB_CONTAINER_ENV) {
        Location::Local(LocalReason::InsideBuilder)
    } else if is_set(env, CI_ENV) {
        Location::Local(LocalReason::Ci)
    } else if env.get(LOCAL_ENV).is_some_and(|value| value == "1") {
        Location::Local(LocalReason::ForcedByEnv)
    } else if remote.mode == RemoteMode::Local {
        Location::Local(LocalReason::ForcedByConfig)
    } else {
        Location::Remote
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

    #[test]
    fn remote_is_the_default() {
        assert_eq!(
            decide_location(&env(&[]), &RemoteConfig::default()),
            Location::Remote
        );
    }

    #[test]
    fn each_signal_selects_local() {
        let rrb = RemoteConfig::default();
        assert_eq!(
            decide_location(&env(&[("RRB_CONTAINER", "1")]), &rrb),
            Location::Local(LocalReason::InsideBuilder)
        );
        assert_eq!(
            decide_location(&env(&[("CI", "true")]), &rrb),
            Location::Local(LocalReason::Ci)
        );
        assert_eq!(
            decide_location(&env(&[("SEKVENT_LOCAL", "1")]), &rrb),
            Location::Local(LocalReason::ForcedByEnv)
        );
        let local = RemoteConfig {
            mode: RemoteMode::Local,
            ..RemoteConfig::default()
        };
        assert_eq!(
            decide_location(&env(&[]), &local),
            Location::Local(LocalReason::ForcedByConfig)
        );
    }

    #[test]
    fn empty_or_other_values_do_not_count() {
        let rrb = RemoteConfig::default();
        for pairs in [
            &[("RRB_CONTAINER", "")][..],
            &[("CI", "")][..],
            &[("SEKVENT_LOCAL", "0")][..],
            &[("SEKVENT_LOCAL", "yes")][..],
        ] {
            assert_eq!(
                decide_location(&env(pairs), &rrb),
                Location::Remote,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn the_builder_wins_over_other_signals() {
        let pairs = [("CI", "1"), ("RRB_CONTAINER", "1"), ("SEKVENT_LOCAL", "1")];
        assert_eq!(
            decide_location(&env(&pairs), &RemoteConfig::default()),
            Location::Local(LocalReason::InsideBuilder)
        );
    }
}
