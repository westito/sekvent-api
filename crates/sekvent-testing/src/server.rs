#![cfg_attr(not(any(feature = "postgres", feature = "mysql")), allow(dead_code))]

use std::future::Future;
use std::time::Duration;

use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use crate::{Harness, HarnessError, Reaper, mapped_addr};

/// Environment variable that opts in to tests needing a Docker daemon.
pub const DOCKER_TESTS_ENV: &str = "SEKVENT_DOCKER_TESTS";

/// Whether [`DOCKER_TESTS_ENV`] is set to `1` or `true`.
///
/// Docker-backed tests are `#[ignore]`d and should also return early when
/// this is false, so `cargo test -- --ignored` without a daemon stays green.
pub fn docker_tests_enabled() -> bool {
    std::env::var(DOCKER_TESTS_ENV).is_ok_and(|value| flag_enabled(&value))
}

fn flag_enabled(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true")
}

/// The container image of a database server, as `name:tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerImage {
    /// Image name, e.g. `postgres`.
    pub name: String,
    /// Image tag, e.g. `17-alpine`.
    pub tag: String,
}

impl ServerImage {
    /// An image from its name and tag.
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
        }
    }

    /// Parse `name:tag`. A reference without a tag uses `latest`; a registry
    /// port (`host:5000/name`) is not mistaken for a tag.
    pub fn parse(reference: &str) -> Self {
        let reference = reference.trim();
        match reference.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') && !name.is_empty() => Self::new(name, tag),
            _ => Self::new(reference, "latest"),
        }
    }

    /// The image named by environment variable `var`, or `default`.
    pub(crate) fn from_env_or(var: &str, default: &Self) -> Self {
        match std::env::var(var) {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            _ => default.clone(),
        }
    }
}

/// A database created for one test.
///
/// Its `Debug` output shows the name only: the URL carries the server's
/// admin password.
#[derive(Clone, PartialEq, Eq)]
pub struct TestDatabase {
    /// The database name, unique per call.
    pub name: String,
    /// A URL connecting to it with the server's admin credentials.
    pub url: String,
}

impl std::fmt::Debug for TestDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestDatabase")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Everything needed to start one database server container.
pub(crate) struct ServerSpec<'a> {
    pub(crate) image: &'a ServerImage,
    pub(crate) port: u16,
    pub(crate) ready_log: WaitFor,
    pub(crate) env: Vec<(&'static str, String)>,
    pub(crate) cmd: Vec<String>,
    pub(crate) startup_timeout: Duration,
}

/// A started, labelled, reaped server container.
pub(crate) struct StartedServer {
    pub(crate) container: ContainerAsync<GenericImage>,
    pub(crate) reaper: Reaper,
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// Start the container, attach its reaper, resolve its address.
///
/// A process killed while `start` is still waiting for readiness leaves the
/// container without a reaper; the labels make it visible to the sweep.
pub(crate) async fn start_server(
    harness: &Harness,
    spec: ServerSpec<'_>,
) -> Result<StartedServer, HarnessError> {
    let port = ContainerPort::Tcp(spec.port);
    let image = GenericImage::new(spec.image.name.clone(), spec.image.tag.clone())
        .with_exposed_port(port)
        .with_wait_for(spec.ready_log);
    let mut request = image
        .with_labels(harness.labels())
        .with_startup_timeout(spec.startup_timeout);
    for (key, value) in spec.env {
        request = request.with_env_var(key, value);
    }
    let container = request.with_cmd(spec.cmd).start().await?;
    let reaper = Reaper::spawn(container.id())?;
    let (host, port) = mapped_addr(&container, port).await?;
    tracing::debug!(
        image = %spec.image.name,
        container = %container.id(),
        run = %harness.run_id(),
        "database container started"
    );
    Ok(StartedServer {
        container,
        reaper,
        host,
        port,
    })
}

/// A fresh admin password for one server container: 32 random hex digits,
/// safe in a URL and in an environment variable.
pub(crate) fn random_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A unique database name: `t_` plus 32 hex digits, valid unquoted in both
/// Postgres and MySQL.
pub(crate) fn unique_database_name() -> String {
    format!("t_{}", uuid::Uuid::new_v4().simple())
}

/// Retry `attempt` until it succeeds or `budget` is spent, doubling the pause
/// from 50 ms up to 1 s. The last error's description is reported, never the
/// URL being connected to.
pub(crate) async fn retry_until_ready<T, E, F, Fut>(
    budget: Duration,
    mut attempt: F,
) -> Result<T, HarnessError>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut pause = Duration::from_millis(50);
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(HarnessError::NotReady(error.to_string()));
                }
                tokio::time::sleep(pause.min(deadline - now)).await;
                pause = (pause * 2).min(Duration::from_secs(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn the_docker_flag_accepts_one_and_true_only() {
        assert!(flag_enabled("1"));
        assert!(flag_enabled(" TRUE "));
        assert!(!flag_enabled("0"));
        assert!(!flag_enabled("yes"));
        assert!(!flag_enabled(""));
        let _ = docker_tests_enabled();
    }

    #[test]
    fn image_references_parse_name_and_tag() {
        assert_eq!(
            ServerImage::parse("postgres:17-alpine"),
            ServerImage::new("postgres", "17-alpine")
        );
        assert_eq!(
            ServerImage::parse("mysql"),
            ServerImage::new("mysql", "latest")
        );
        assert_eq!(
            ServerImage::parse("registry.lan:5000/db/postgres"),
            ServerImage::new("registry.lan:5000/db/postgres", "latest")
        );
        assert_eq!(
            ServerImage::parse("registry.lan:5000/db/postgres:16"),
            ServerImage::new("registry.lan:5000/db/postgres", "16")
        );
        assert_eq!(
            ServerImage::parse(":tag"),
            ServerImage::new(":tag", "latest")
        );
    }

    #[test]
    fn an_unset_image_variable_keeps_the_default() {
        let default = ServerImage::new("postgres", "17-alpine");
        assert_eq!(
            ServerImage::from_env_or("SEKVENT_TEST_UNSET_IMAGE_VARIABLE", &default),
            default
        );
    }

    #[test]
    fn database_names_are_unique_and_portable() {
        let first = unique_database_name();
        assert_ne!(first, unique_database_name());
        assert_eq!(first.len(), 34);
        assert!(first.starts_with("t_"));
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }

    #[test]
    fn passwords_are_random_hex() {
        let first = random_password();
        assert_ne!(first, random_password());
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_databases_debug_without_the_url() {
        let database = TestDatabase {
            name: "t_1".to_owned(),
            url: "postgres://sekvent:hunter2@db:5432/t_1".to_owned(),
        };
        let shown = format!("{database:?}");
        assert_eq!(shown, r#"TestDatabase { name: "t_1", .. }"#);
        assert!(!shown.contains("hunter2"));
        assert_eq!(database.clone(), database);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_an_attempt_succeeds() {
        let calls = Cell::new(0);
        let start = tokio::time::Instant::now();
        let value = retry_until_ready(Duration::from_secs(5), || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move { if n < 3 { Err("refused") } else { Ok(n) } }
        })
        .await
        .unwrap();
        assert_eq!(value, 3);
        assert_eq!(start.elapsed(), Duration::from_millis(150));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_with_the_last_error_after_the_budget() {
        let calls = Cell::new(0);
        let error = retry_until_ready::<(), _, _, _>(Duration::from_millis(120), || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move { Err(format!("attempt {n} refused")) }
        })
        .await
        .unwrap_err();
        // Pauses of 50 and 70 (capped by the budget) ms, then a final attempt.
        assert_eq!(calls.get(), 3);
        assert_eq!(
            error.to_string(),
            "database server did not become ready: attempt 3 refused"
        );
    }
}
