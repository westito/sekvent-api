use std::time::Duration;

use sqlx::{AssertSqlSafe, Connection, PgConnection};
use testcontainers::core::WaitFor;
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::{ContainerAsync, GenericImage};
use tokio::sync::OnceCell;

use crate::addr::server_url;
use crate::server::{ServerSpec, retry_until_ready, start_server, unique_database_name};
use crate::{Harness, HarnessError, Reaper, ServerImage, TestDatabase};

const PORT: u16 = 5432;
const USER: &str = "sekvent";
const PASSWORD: &str = "sekvent";
const ADMIN_DATABASE: &str = "postgres";

/// Overrides the shared container's image (`name:tag`).
const IMAGE_ENV: &str = "SEKVENT_TEST_POSTGRES_IMAGE";

/// Settings that trade durability for speed. A test database is disposable,
/// so skipping fsync and full-page writes costs nothing; many parallel tests
/// each hold a small pool, hence the raised connection limit; `mmap` shared
/// memory avoids the container's small default `/dev/shm`; parallel workers
/// add latency without benefit on tiny tables.
const SERVER_SETTINGS: [&str; 7] = [
    "fsync=off",
    "synchronous_commit=off",
    "full_page_writes=off",
    "max_connections=500",
    "dynamic_shared_memory_type=mmap",
    "max_parallel_workers_per_gather=0",
    "max_parallel_maintenance_workers=0",
];

static SHARED: OnceCell<PostgresHarness> = OnceCell::const_new();

/// A Postgres server in a container, with one database per test.
///
/// Use [`PostgresHarness::shared`] in tests: every test in a binary shares
/// one container and calls [`PostgresHarness::create_database`] for its own
/// database, which keeps tests isolated without paying a container start
/// each.
pub struct PostgresHarness {
    _container: ContainerAsync<GenericImage>,
    _reaper: Reaper,
    host: String,
    port: u16,
    image: ServerImage,
}

impl std::fmt::Debug for PostgresHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresHarness")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("image", &self.image)
            .finish_non_exhaustive()
    }
}

impl PostgresHarness {
    /// The default image, `postgres:17-alpine`.
    pub fn default_image() -> ServerImage {
        ServerImage::new("postgres", "17-alpine")
    }

    /// The per-binary shared server, started on first use with
    /// [`Harness::from_env`] and the image from `SEKVENT_TEST_POSTGRES_IMAGE`
    /// or [`PostgresHarness::default_image`].
    ///
    /// It lives until the test process exits, when its [`Reaper`] removes it.
    ///
    /// # Panics
    ///
    /// When the container cannot be started; a test cannot proceed anyway.
    pub async fn shared() -> &'static Self {
        SHARED
            .get_or_init(|| async {
                let harness = Harness::from_env().expect("valid harness settings");
                let image = ServerImage::from_env_or(IMAGE_ENV, &Self::default_image());
                Self::start(&harness, image)
                    .await
                    .expect("start the shared Postgres container")
            })
            .await
    }

    /// Start a dedicated server. The container is removed when the returned
    /// value is dropped or, failing that, when the process exits.
    pub async fn start(harness: &Harness, image: ServerImage) -> Result<Self, HarnessError> {
        let spec = ServerSpec {
            image: &image,
            port: PORT,
            // The entrypoint runs a temporary server during initialisation
            // and prints the message once for it and once for the real one.
            ready_log: WaitFor::log(
                LogWaitStrategy::stderr("database system is ready to accept connections")
                    .with_times(2),
            ),
            env: vec![
                ("POSTGRES_USER", USER.to_owned()),
                ("POSTGRES_PASSWORD", PASSWORD.to_owned()),
                ("POSTGRES_DB", ADMIN_DATABASE.to_owned()),
            ],
            cmd: server_command(),
            startup_timeout: Duration::from_secs(120),
        };
        let started = start_server(harness, spec).await?;
        let this = Self {
            _container: started.container,
            _reaper: started.reaper,
            host: started.host,
            port: started.port,
            image,
        };
        let admin = this.admin_url();
        let conn =
            retry_until_ready(Duration::from_secs(30), || PgConnection::connect(&admin)).await?;
        conn.close().await.map_err(HarnessError::Database)?;
        Ok(this)
    }

    /// The server URL without a database path.
    pub fn url(&self) -> String {
        server_url("postgres", USER, PASSWORD, &self.host, self.port, "")
    }

    /// The URL of the admin database (`postgres`).
    pub fn admin_url(&self) -> String {
        self.url_for(ADMIN_DATABASE)
    }

    /// The URL of database `name` on this server.
    pub fn url_for(&self, name: &str) -> String {
        server_url("postgres", USER, PASSWORD, &self.host, self.port, name)
    }

    /// The published host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The published port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Create a fresh, uniquely named, empty database.
    pub async fn create_database(&self) -> Result<TestDatabase, HarnessError> {
        let name = unique_database_name();
        let mut conn = PgConnection::connect(&self.admin_url())
            .await
            .map_err(HarnessError::Database)?;
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&mut conn)
            .await
            .map_err(HarnessError::Database)?;
        conn.close().await.map_err(HarnessError::Database)?;
        let url = self.url_for(&name);
        Ok(TestDatabase { name, url })
    }
}

fn server_command() -> Vec<String> {
    let mut cmd = vec!["postgres".to_owned()];
    for setting in SERVER_SETTINGS {
        cmd.push("-c".to_owned());
        cmd.push(setting.to_owned());
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_runs_with_test_tuned_settings() {
        let cmd = server_command();
        assert_eq!(cmd[0], "postgres");
        assert_eq!(cmd.len(), 1 + 2 * SERVER_SETTINGS.len());
        for setting in [
            "fsync=off",
            "synchronous_commit=off",
            "full_page_writes=off",
            "max_connections=500",
            "dynamic_shared_memory_type=mmap",
            "max_parallel_workers_per_gather=0",
            "max_parallel_maintenance_workers=0",
        ] {
            let at = cmd.iter().position(|arg| arg == setting).unwrap();
            assert_eq!(cmd[at - 1], "-c");
        }
    }

    #[test]
    fn the_default_image_is_postgres_17_alpine() {
        assert_eq!(
            PostgresHarness::default_image(),
            ServerImage::parse("postgres:17-alpine")
        );
    }
}
