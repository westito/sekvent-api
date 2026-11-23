//! The admin audit record: a row per token id, written before the body runs.

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sekvent_context::Clock;
use sekvent_db::{constraint_name, to_app_error};
use sekvent_error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

use super::claims::AdminClaims;
use crate::ErrorDomains;

/// The unique key whose violation is a replayed token.
const PRIMARY_KEY: &str = "admin_audit_pkey";

/// One row per token id, outcome `started`.
const RECORD: &str = "INSERT INTO admin_audit \
     (token_id, occurred_at, operation, actor, outcome, tenant_id) \
     VALUES ($1, $2, $3, $4, 'started', $5)";

/// The outcome of a started row.
const FINISH: &str =
    "UPDATE admin_audit SET outcome = $2 WHERE token_id = $1 AND outcome = 'started'";

/// Writes `admin_audit` on the control database. Clones share the pool. `Debug` prints the type
/// name only.
#[derive(Clone)]
pub struct AdminAudit {
    control: PgPool,
    clock: Arc<dyn Clock>,
    domains: ErrorDomains,
}

impl fmt::Debug for AdminAudit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminAudit(..)")
    }
}

impl AdminAudit {
    /// The audit on the control pool, with the time of `clock` and the default domains.
    pub fn new(control: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self {
            control,
            clock,
            domains: ErrorDomains::new(),
        }
    }

    /// The domains the guard answers in (the admin one).
    #[must_use]
    pub fn with_domains(self, domains: ErrorDomains) -> Self {
        Self { domains, ..self }
    }

    /// The domains the guard answers in.
    pub fn domains(&self) -> ErrorDomains {
        self.domains
    }

    /// Records the token id with the time, the operation, the subject and the tenant, outcome
    /// `started`. A second record of one id is `ADMIN_TOKEN_REPLAYED` in the admin domain; any
    /// other failure is the database's error.
    pub(crate) async fn record<X>(&self, claims: &AdminClaims<X>) -> Result<(), AppError> {
        sqlx::query(RECORD)
            .bind(claims.token_id)
            .bind(DateTime::<Utc>::from(self.clock.now()))
            .bind(claims.op.as_str())
            .bind(claims.subject.as_str())
            .bind(claims.tenant.map(Uuid::from))
            .execute(&self.control)
            .await
            .map(|_| ())
            .map_err(|error| {
                if constraint_name(&error) == Some(PRIMARY_KEY) {
                    crate::error::admin::token_replayed(self.domains.admin)
                } else {
                    to_app_error(error)
                }
            })
    }

    /// Stores the outcome of the call of `token_id`: `ok` or `failed`. A failure to store it is a
    /// warning with the token id and the error's code only.
    pub(crate) async fn finish(&self, token_id: Uuid, ok: bool) {
        let outcome = if ok { "ok" } else { "failed" };
        let stored = sqlx::query(FINISH)
            .bind(token_id)
            .bind(outcome)
            .execute(&self.control)
            .await;
        if let Err(error) = stored {
            let code = to_app_error(error).code();
            tracing::warn!(
                token_id = %token_id,
                code = ?code,
                "the outcome of an admin call was not stored"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sekvent_context::SystemClock;
    use sqlx::PgPool;

    use super::AdminAudit;
    use crate::ErrorDomains;

    #[tokio::test]
    async fn debug_prints_nothing_and_the_domains_are_kept() {
        // A lazy pool never connects; nothing here touches a database.
        let pool = PgPool::connect_lazy("postgres://u@127.0.0.1:1/orders").unwrap();
        let audit = AdminAudit::new(pool, Arc::new(SystemClock));
        assert_eq!(format!("{audit:?}"), "AdminAudit(..)");
        assert_eq!(audit.domains(), ErrorDomains::new());
        let audit = audit.with_domains(ErrorDomains::new().with_admin("billing.admin.v1"));
        assert_eq!(audit.domains().admin, "billing.admin.v1");
    }
}
