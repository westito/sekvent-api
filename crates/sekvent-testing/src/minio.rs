use std::time::Duration;

use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::core::{ExecCommand, WaitFor};
use testcontainers::{ContainerAsync, GenericImage};
use tokio::sync::OnceCell;

use crate::server::{ServerSpec, random_password, retry_until_ready, start_server};
use crate::{Harness, HarnessError, Reaper, ServerImage};

const PORT: u16 = 9000;
const USER: &str = "sekvent";
const REGION: &str = "us-east-1";

/// Overrides the shared container's image (`name:tag`).
const IMAGE_ENV: &str = "SEKVENT_TEST_MINIO_IMAGE";

/// The `mc` alias the harness defines for its own server.
const ALIAS: &str = "local";

/// Longest command output kept in an error.
const OUTPUT_LIMIT: usize = 300;

static SHARED: OnceCell<MinioHarness> = OnceCell::const_new();

/// A `MinIO` server in a container, with one bucket per test.
///
/// Use [`MinioHarness::shared`] in tests: every test in a binary shares one
/// container and calls [`MinioHarness::create_bucket`] for its own bucket.
/// The root credentials are random per container; buckets are created with
/// `mc mb` inside the container, so the image must carry `mc`.
pub struct MinioHarness {
    container: ContainerAsync<GenericImage>,
    _reaper: Reaper,
    host: String,
    port: u16,
    password: String,
    image: ServerImage,
}

impl std::fmt::Debug for MinioHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinioHarness")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("image", &self.image)
            .finish_non_exhaustive()
    }
}

/// A bucket created for one test, with everything a client needs.
///
/// Its `Debug` output omits the secret key.
#[derive(Clone, PartialEq, Eq)]
pub struct TestBucket {
    /// The bucket name, unique per call.
    pub name: String,
    /// `http://host:port` of the server.
    pub endpoint: String,
    /// The region to sign for (`us-east-1`).
    pub region: String,
    /// The access key id (the server's root user).
    pub access_key_id: String,
    /// The secret access key (the server's random root password).
    pub secret_access_key: String,
}

impl std::fmt::Debug for TestBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestBucket")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl TestBucket {
    /// The settings `sekvent-storage`'s `StorageConfig` reads, under
    /// `prefix`: `ENDPOINT`, `REGION`, `BUCKET`, `ACCESS_KEY_ID`,
    /// `SECRET_ACCESS_KEY` and `PATH_STYLE=true`. Collect them into a
    /// `MapSource`.
    pub fn config(&self, prefix: &str) -> Vec<(String, String)> {
        [
            ("ENDPOINT", self.endpoint.as_str()),
            ("REGION", self.region.as_str()),
            ("BUCKET", self.name.as_str()),
            ("ACCESS_KEY_ID", self.access_key_id.as_str()),
            ("SECRET_ACCESS_KEY", self.secret_access_key.as_str()),
            ("PATH_STYLE", "true"),
        ]
        .into_iter()
        .map(|(key, value)| (format!("{prefix}{key}"), value.to_owned()))
        .collect()
    }
}

impl MinioHarness {
    /// The default image, `pgsty/minio:RELEASE.2026-08-04T00-00-00Z`: a
    /// maintained community build of the `MinIO` server, which no longer
    /// publishes images of its own.
    pub fn default_image() -> ServerImage {
        ServerImage::new("pgsty/minio", "RELEASE.2026-08-04T00-00-00Z")
    }

    /// The per-binary shared server, started on first use with
    /// [`Harness::from_env`] and the image from `SEKVENT_TEST_MINIO_IMAGE`
    /// or [`MinioHarness::default_image`].
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
                    .expect("start the shared MinIO container")
            })
            .await
    }

    /// Start a dedicated server. The container is removed when the returned
    /// value is dropped or, failing that, when the process exits.
    pub async fn start(harness: &Harness, image: ServerImage) -> Result<Self, HarnessError> {
        let password = random_password();
        let spec = ServerSpec {
            image: &image,
            port: PORT,
            ready_log: WaitFor::log(LogWaitStrategy::stdout_or_stderr("API:")),
            env: vec![
                ("MINIO_ROOT_USER", USER.to_owned()),
                ("MINIO_ROOT_PASSWORD", password.clone()),
                ("MINIO_BROWSER", "off".to_owned()),
            ],
            cmd: vec!["server".to_owned(), "/data".to_owned()],
            startup_timeout: Duration::from_secs(120),
        };
        let started = start_server(harness, spec).await?;
        let this = Self {
            container: started.container,
            _reaper: started.reaper,
            host: started.host,
            port: started.port,
            password,
            image,
        };
        retry_until_ready(Duration::from_secs(30), || this.mc(&["ls", ALIAS])).await?;
        Ok(this)
    }

    /// `http://host:port` of the server.
    pub fn endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// The published host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The published port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The region to sign for.
    pub fn region(&self) -> &str {
        REGION
    }

    /// The root user, used as access key id.
    pub fn access_key_id(&self) -> &str {
        USER
    }

    /// The random root password, used as secret access key.
    pub fn secret_access_key(&self) -> &str {
        &self.password
    }

    /// Create a fresh, uniquely named, empty bucket.
    pub async fn create_bucket(&self) -> Result<TestBucket, HarnessError> {
        let name = unique_bucket_name();
        self.mc(&["mb", &format!("{ALIAS}/{name}")]).await?;
        Ok(TestBucket {
            name,
            endpoint: self.endpoint(),
            region: REGION.to_owned(),
            access_key_id: USER.to_owned(),
            secret_access_key: self.password.clone(),
        })
    }

    /// Run `mc` with `args` inside the container against this server.
    async fn mc(&self, args: &[&str]) -> Result<(), HarnessError> {
        let host = format!("http://{USER}:{}@127.0.0.1:{PORT}", self.password);
        let command = ExecCommand::new(std::iter::once("mc").chain(args.iter().copied()))
            .with_env_vars([(format!("MC_HOST_{ALIAS}"), host)]);
        let mut result = self.container.exec(command).await?;
        let stdout = result.stdout_to_vec().await?;
        let stderr = result.stderr_to_vec().await?;
        // The streams end when the command exits; the daemon may record the
        // exit code a moment later.
        let result = &result;
        let code = retry_until_ready(Duration::from_secs(10), || async move {
            match result.exit_code().await {
                Ok(Some(code)) => Ok(code),
                Ok(None) => Err("the command is still running".to_owned()),
                Err(error) => Err(error.to_string()),
            }
        })
        .await?;
        if code == 0 {
            return Ok(());
        }
        let output = if stderr.is_empty() { stdout } else { stderr };
        Err(HarnessError::Exec(describe_failure(
            code,
            &String::from_utf8_lossy(&output),
            &self.password,
        )))
    }
}

/// A failed command's description: exit code and the start of its output,
/// with the password removed.
fn describe_failure(code: i64, output: &str, password: &str) -> String {
    let cleaned = output.replace(password, "[redacted]");
    let short: String = cleaned.trim().chars().take(OUTPUT_LIMIT).collect();
    format!("exit code {code}: {short}")
}

/// A unique bucket name: `t-` plus 32 hex digits, valid under S3's rules.
fn unique_bucket_name() -> String {
    format!("t-{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> TestBucket {
        TestBucket {
            name: "t-1".to_owned(),
            endpoint: "http://127.0.0.1:9000".to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: "sekvent".to_owned(),
            secret_access_key: "hunter2".to_owned(),
        }
    }

    #[test]
    fn the_default_image_is_a_pinned_minio_release() {
        let image = MinioHarness::default_image();
        assert_eq!(image.name, "pgsty/minio");
        assert!(image.tag.starts_with("RELEASE."));
    }

    #[test]
    fn bucket_names_are_unique_and_valid() {
        let first = unique_bucket_name();
        assert_ne!(first, unique_bucket_name());
        assert_eq!(first.len(), 34);
        assert!(first.starts_with("t-"));
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
    }

    #[test]
    fn buckets_render_storage_settings_under_a_prefix() {
        let config = bucket().config("FILES_");
        assert_eq!(
            config,
            [
                ("FILES_ENDPOINT", "http://127.0.0.1:9000"),
                ("FILES_REGION", "us-east-1"),
                ("FILES_BUCKET", "t-1"),
                ("FILES_ACCESS_KEY_ID", "sekvent"),
                ("FILES_SECRET_ACCESS_KEY", "hunter2"),
                ("FILES_PATH_STYLE", "true"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
    }

    #[test]
    fn buckets_debug_without_the_secret() {
        let bucket = bucket();
        let shown = format!("{bucket:?}");
        assert!(shown.contains("t-1"));
        assert!(!shown.contains("hunter2"));
        assert_eq!(bucket.clone(), bucket);
    }

    #[test]
    fn failures_hide_the_password_and_stay_short() {
        let text = describe_failure(1, "  mc: <ERROR> http://sekvent:pw123@x failed\n", "pw123");
        assert_eq!(
            text,
            "exit code 1: mc: <ERROR> http://sekvent:[redacted]@x failed"
        );
        let long = describe_failure(2, &"x".repeat(1000), "pw");
        assert_eq!(long.len(), "exit code 2: ".len() + OUTPUT_LIMIT);
    }
}
