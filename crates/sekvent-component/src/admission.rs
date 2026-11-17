//! The admission of calls served over gRPC: how many calls that no link
//! token authenticated may be read, resolved and authenticated at once, and
//! how large and how slow a request may be before its scope is resolved.

use std::time::Duration;

/// Bounds on what a call served over gRPC or gRPC-Web may cost before its
/// request scope is resolved and its end user authenticated.
///
/// Set once per App with
/// [`AppBuilder::serve_admission`](crate::AppBuilder::serve_admission); every
/// component the App serves shares them, so one listener mounting the App's
/// routes has one admission. Without a call the defaults apply.
///
/// - **`max_pending`** (default [`DEFAULT_MAX_PENDING`](Self::DEFAULT_MAX_PENDING)):
///   how many calls that no link token authenticated may be between their
///   admission and the component's gate at once, across every method of
///   every served component: reading their request, resolving their scope,
///   authenticating their end user. One more is refused at once with
///   `RESOURCE_EXHAUSTED` / [`ADMISSION_FULL`](crate::reasons::ADMISSION_FULL).
///   Link callers never count.
/// - **`max_pending_per_client`** (default 32): the simultaneous calls one
///   client address may hold within that shared admission. Uses the address
///   limiter's canonical IPv4, IPv6 /64 or shared unknown-address key. Both
///   counts are released on cancellation and idle address entries are removed.
///   Behind a gateway, configure trusted client-address forwarding and have
///   the gateway buffer bodies or enforce its own body deadlines; a rate
///   limit alone does not bound slow calls in flight.
/// - **`read_timeout`** (default [`DEFAULT_READ_TIMEOUT`](Self::DEFAULT_READ_TIMEOUT)):
///   how long the request of a call may take to arrive whole, for every
///   caller. Past it the call is answered `DEADLINE_EXCEEDED` /
///   [`REQUEST_READ_TIMEOUT`](crate::reasons::REQUEST_READ_TIMEOUT); the
///   call's own deadline still applies when it is earlier.
/// - **`max_message_bytes`** (default and largest
///   [`MAX_MESSAGE_BYTES`](Self::MAX_MESSAGE_BYTES)): the largest request
///   message a served method accepts. A larger request is refused with
///   `OUT_OF_RANGE` / [`REQUEST_TOO_LARGE`](crate::reasons::REQUEST_TOO_LARGE)
///   as soon as that is known, without reading the rest.
///
/// ```
/// use std::time::Duration;
/// use sekvent_component::ServeAdmission;
///
/// let admission = ServeAdmission::new()
///     .with_max_pending(256)
///     .with_read_timeout(Duration::from_secs(5))
///     .with_max_message_bytes(1024 * 1024);
/// assert_eq!(admission.max_pending(), 256);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeAdmission {
    max_pending: usize,
    max_pending_per_client: usize,
    read_timeout: Duration,
    max_message_bytes: usize,
}

impl ServeAdmission {
    /// Default of `max_pending`.
    pub const DEFAULT_MAX_PENDING: usize = 512;
    /// Default of `max_pending_per_client`.
    pub const DEFAULT_MAX_PENDING_PER_CLIENT: usize = 32;
    /// Largest request read timeout (5 minutes).
    pub const MAX_READ_TIMEOUT: Duration = Duration::from_secs(300);
    /// Default of `read_timeout`.
    pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
    /// Default and largest `max_message_bytes` (4 MiB).
    pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

    /// The defaults.
    pub const fn new() -> Self {
        Self {
            max_pending: Self::DEFAULT_MAX_PENDING,
            max_pending_per_client: Self::DEFAULT_MAX_PENDING_PER_CLIENT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
            max_message_bytes: Self::MAX_MESSAGE_BYTES,
        }
    }

    /// Admit at most `calls` calls at once (at least 1).
    #[must_use]
    pub const fn with_max_pending(self, calls: usize) -> Self {
        Self {
            max_pending: calls,
            ..self
        }
    }

    /// Admit at most `calls` simultaneous calls per client address (at least 1).
    /// Uses the address limiter's key: canonical IPv4, IPv6 /64, or one
    /// shared unknown-address bucket. Idle entries are removed immediately.
    #[must_use]
    pub const fn with_max_pending_per_client(self, calls: usize) -> Self {
        Self {
            max_pending_per_client: calls,
            ..self
        }
    }

    /// How many simultaneous calls one client address may hold.
    pub const fn max_pending_per_client(&self) -> usize {
        self.max_pending_per_client
    }

    /// Give a request at most `timeout` to arrive whole (more than zero,
    /// at most [`MAX_READ_TIMEOUT`](Self::MAX_READ_TIMEOUT)).
    #[must_use]
    pub const fn with_read_timeout(self, timeout: Duration) -> Self {
        Self {
            read_timeout: timeout,
            ..self
        }
    }

    /// Accept request messages of at most `bytes` bytes (1 to
    /// [`MAX_MESSAGE_BYTES`](Self::MAX_MESSAGE_BYTES)).
    #[must_use]
    pub const fn with_max_message_bytes(self, bytes: usize) -> Self {
        Self {
            max_message_bytes: bytes,
            ..self
        }
    }

    /// How many calls may be admitted at once.
    pub const fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// How long a request may take to arrive whole.
    pub const fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// The largest request message accepted.
    pub const fn max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }

    /// Why these bounds cannot be used, if they cannot.
    pub(crate) fn problem(&self) -> Option<&'static str> {
        if self.max_pending == 0 {
            Some("max_pending must be at least 1")
        } else if self.max_pending > tokio::sync::Semaphore::MAX_PERMITS {
            Some("max_pending is larger than the admission can count")
        } else if self.max_pending_per_client == 0 {
            Some("max_pending_per_client must be at least 1")
        } else if self.read_timeout > Self::MAX_READ_TIMEOUT {
            Some("read_timeout must be at most 5 minutes")
        } else if self.read_timeout.is_zero() {
            Some("read_timeout must be more than zero")
        } else if self.max_message_bytes == 0 {
            Some("max_message_bytes must be at least 1")
        } else if self.max_message_bytes > Self::MAX_MESSAGE_BYTES {
            Some("max_message_bytes must be at most 4 MiB")
        } else {
            None
        }
    }
}

impl Default for ServeAdmission {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_and_the_setters() {
        let admission = ServeAdmission::default();
        assert_eq!(admission, ServeAdmission::new());
        assert_eq!(admission.max_pending(), 512);
        assert_eq!(admission.max_pending_per_client(), 32);
        assert_eq!(admission.read_timeout(), Duration::from_secs(10));
        assert_eq!(admission.max_message_bytes(), 4 * 1024 * 1024);
        assert_eq!(admission.problem(), None);

        let tuned = admission
            .with_max_pending(3)
            .with_max_pending_per_client(2)
            .with_read_timeout(Duration::from_millis(250))
            .with_max_message_bytes(1024);
        assert_eq!(tuned.max_pending(), 3);
        assert_eq!(tuned.max_pending_per_client(), 2);
        assert_eq!(tuned.read_timeout(), Duration::from_millis(250));
        assert_eq!(tuned.max_message_bytes(), 1024);
        assert_eq!(tuned.problem(), None);
        assert!(format!("{tuned:?}").contains("max_pending: 3"));
    }

    #[test]
    fn unusable_bounds_are_named() {
        let base = ServeAdmission::new();
        for (admission, needle) in [
            (base.with_max_pending(0), "max_pending"),
            (
                base.with_max_pending_per_client(0),
                "max_pending_per_client",
            ),
            (base.with_read_timeout(Duration::MAX), "read_timeout"),
            (
                base.with_max_pending(tokio::sync::Semaphore::MAX_PERMITS + 1),
                "max_pending",
            ),
            (base.with_read_timeout(Duration::ZERO), "read_timeout"),
            (base.with_max_message_bytes(0), "max_message_bytes"),
            (
                base.with_max_message_bytes(ServeAdmission::MAX_MESSAGE_BYTES + 1),
                "max_message_bytes",
            ),
        ] {
            let problem = admission.problem().expect("refused");
            assert!(problem.contains(needle), "{problem}");
        }
        assert_eq!(
            base.with_max_pending(tokio::sync::Semaphore::MAX_PERMITS)
                .with_max_message_bytes(1)
                .with_read_timeout(ServeAdmission::MAX_READ_TIMEOUT)
                .problem(),
            None
        );
    }
}
