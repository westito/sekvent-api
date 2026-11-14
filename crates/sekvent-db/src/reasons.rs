/// A lease is no longer held under the caller's owner or fencing token
/// (`ABORTED`). The same value as `sekvent_runtime::reasons::LEASE_LOST`.
pub const LEASE_LOST: &str = "LEASE_LOST";

/// The lease table is absent or lacks a column (`FAILED_PRECONDITION`).
pub const LEASE_SCHEMA_MISSING: &str = "LEASE_SCHEMA_MISSING";

/// The database refused a statement for a missing grant
/// (`PERMISSION_DENIED`).
pub const DB_PERMISSION_DENIED: &str = "DB_PERMISSION_DENIED";

/// The database rejected the service's own credentials (`INTERNAL`): the
/// service is misconfigured and a retry will not help.
pub const DB_CREDENTIALS_REJECTED: &str = "DB_CREDENTIALS_REJECTED";

/// A keyed cache was asked for a new key while every entry it holds is in
/// use or being created (`RESOURCE_EXHAUSTED`).
pub const KEYED_CAPACITY: &str = "KEYED_CAPACITY";

/// Creating a keyed cache's value outlived its `create_timeout`
/// (`UNAVAILABLE`).
pub const KEYED_CREATE_TIMEOUT: &str = "KEYED_CREATE_TIMEOUT";

/// A keyed cache's factory panicked (`INTERNAL`).
pub const KEYED_FACTORY_PANICKED: &str = "KEYED_FACTORY_PANICKED";

/// The keyed cache is closed (`UNAVAILABLE`): a `get` after `close`, and the
/// waiters of a creation that `close` cancelled.
pub const KEYED_CLOSED: &str = "KEYED_CLOSED";
