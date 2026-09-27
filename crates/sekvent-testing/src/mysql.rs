use std::time::Duration;

use sqlx::{AssertSqlSafe, Connection, MySqlConnection};
use testcontainers::core::WaitFor;
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::{ContainerAsync, GenericImage};
use tokio::sync::OnceCell;

use crate::addr::server_url;
use crate::server::{ServerSpec, retry_until_ready, start_server, unique_database_name};
use crate::{Harness, HarnessError, Reaper, ServerImage, TestDatabase};

const PORT: u16 = 3306;
const USER: &str = "root";
const PASSWORD: &str = "sekvent";
const ADMIN_DATABASE: &str = "mysql";

/// Overrides the shared container's image (`name:tag`).
const IMAGE_ENV: &str = "SEKVENT_TEST_MYSQL_IMAGE";

/// Server flags for disposable data: no binary log, no flush per commit, and
/// room for many parallel test pools.
const SERVER_FLAGS: [&str; 3] = [
    "--skip-log-bin",
    "--innodb-flush-log-at-trx-commit=0",
    "--max-connections=500",
];

static SHARED: OnceCell<MySqlHarness> = OnceCell::const_new();

/// A MySQL server in a container, with one database per test.
///
/// Use [`MySqlHarness::shared`] in tests: every test in a binary shares one
/// container and calls [`MySqlHarness::create_database`] for its own
/// database.
pub struct MySqlHarness {
    _container: ContainerAsync<GenericImage>,
    _reaper: Reaper,
    host: String,
    port: u16,
    image: ServerImage,
}

impl std::fmt::Debug for MySqlHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlHarness")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("image", &self.image)
            .finish_non_exhaustive()
    }
}

impl MySqlHarness {
    /// The default image, `mysql:8.4`.
    pub fn default_image() -> ServerImage {
        ServerImage::new("mysql", "8.4")
    }

    /// The per-binary shared server, started on first use with
    /// [`Harness::from_env`] and the image from `SEKVENT_TEST_MYSQL_IMAGE`
    /// or [`MySqlHarness::default_image`].
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
                    .expect("start the shared MySQL container")
            })
            .await
    }

    /// Start a dedicated server. The container is removed when the returned
    /// value is dropped or, failing that, when the process exits.
    pub async fn start(harness: &Harness, image: ServerImage) -> Result<Self, HarnessError> {
        let spec = ServerSpec {
            image: &image,
            port: PORT,
            // The entrypoint's temporary initialisation server also reports
            // readiness, so the real server is the second occurrence.
            ready_log: WaitFor::log(
                LogWaitStrategy::stdout_or_stderr("ready for connections").with_times(2),
            ),
            env: vec![("MYSQL_ROOT_PASSWORD", PASSWORD.to_owned())],
            cmd: SERVER_FLAGS.iter().map(|flag| (*flag).to_owned()).collect(),
            startup_timeout: Duration::from_secs(180),
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
            retry_until_ready(Duration::from_secs(60), || MySqlConnection::connect(&admin)).await?;
        conn.close().await.map_err(HarnessError::Database)?;
        Ok(this)
    }

    /// The server URL without a database path.
    pub fn url(&self) -> String {
        server_url("mysql", USER, PASSWORD, &self.host, self.port, "")
    }

    /// The URL of the admin database (`mysql`).
    pub fn admin_url(&self) -> String {
        self.url_for(ADMIN_DATABASE)
    }

    /// The URL of database `name` on this server.
    pub fn url_for(&self, name: &str) -> String {
        server_url("mysql", USER, PASSWORD, &self.host, self.port, name)
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
        let mut conn = MySqlConnection::connect(&self.admin_url())
            .await
            .map_err(HarnessError::Database)?;
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE `{name}`")))
            .execute(&mut conn)
            .await
            .map_err(HarnessError::Database)?;
        conn.close().await.map_err(HarnessError::Database)?;
        let url = self.url_for(&name);
        Ok(TestDatabase { name, url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_runs_with_test_tuned_flags() {
        assert_eq!(
            SERVER_FLAGS,
            [
                "--skip-log-bin",
                "--innodb-flush-log-at-trx-commit=0",
                "--max-connections=500"
            ]
        );
    }

    #[test]
    fn the_default_image_is_mysql_8_4() {
        assert_eq!(
            MySqlHarness::default_image(),
            ServerImage::parse("mysql:8.4")
        );
    }
}
