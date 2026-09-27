/// Why the harness could not do what a test asked.
///
/// Messages never contain connection URLs or passwords.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HarnessError {
    /// A label namespace, run id or container id has an unusable shape.
    #[error("invalid {what}: {reason}")]
    Invalid {
        /// Which input was rejected.
        what: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Starting or inspecting a container failed.
    #[error("container operation failed: {0}")]
    Container(#[from] testcontainers::TestcontainersError),
    /// Running the Docker CLI failed.
    #[error("docker CLI failed: {0}")]
    Docker(String),
    /// A local process operation (spawning the reaper, for example) failed.
    #[error("process error: {0}")]
    Io(#[from] std::io::Error),
    /// The database never accepted a connection within the startup budget.
    #[error("database server did not become ready: {0}")]
    NotReady(String),
    /// A database statement failed.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    #[error("database statement failed: {0}")]
    Database(#[source] sqlx::Error),
}
