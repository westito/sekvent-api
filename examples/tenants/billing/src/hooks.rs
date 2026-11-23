//! The billing product's side of the tenant lifecycle.

use std::collections::BTreeMap;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use sekvent::db::to_app_error;
use sekvent::prelude::*;
use sekvent::tenancy::TenantInfo;
use sekvent::tenancy::lifecycle::{
    Description, HookFuture, SeedOutcome, TenantHooks, UsageCounters,
};
use sqlx::PgPool;
use uuid::Uuid;

/// What a new tenant is seeded with. `Debug` leaves the owner's address out.
#[derive(Clone, PartialEq, Eq)]
pub struct BillingSeed {
    /// Who is invited to take the tenant over.
    pub owner_email: String,
    /// The currency of the default invoice series: three capital letters.
    pub currency: String,
}

impl std::fmt::Debug for BillingSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingSeed")
            .field("currency", &self.currency)
            .finish_non_exhaustive()
    }
}

/// The billing product's hooks. Adoption keeps the default, which refuses
/// every database: this product only provisions.
#[derive(Debug, Default, Clone, Copy)]
pub struct BillingHooks;

/// The default series, created by the seed.
const SERIES: &str = "INSERT INTO invoice_series (name, prefix, currency) \
     VALUES ('default', 'INV-', $1) ON CONFLICT (name) DO NOTHING";

/// The owner's invitation; only the token's SHA-256 is stored.
const INVITE: &str = "INSERT INTO owner_invites (token_sha256, email) \
     VALUES (encode(sha256(convert_to($1, 'UTF8')), 'hex'), $2)";

/// A fresh invitation token: two v4 UUIDs (244 random bits) as 64 hex digits.
fn owner_token() -> Secret {
    Secret::new(format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ))
}

/// Creates the default series and the owner's invitation in one
/// transaction. When the series is there already (an earlier seed
/// committed), changes nothing and hands nothing over.
async fn seed_tenant(pool: &PgPool, seed: &BillingSeed) -> Result<SeedOutcome, AppError> {
    let mut tx = pool.begin().await.map_err(to_app_error)?;
    let created = sqlx::query(SERIES)
        .bind(seed.currency.as_str())
        .execute(&mut *tx)
        .await
        .map_err(to_app_error)?
        .rows_affected();
    if created == 0 {
        tx.rollback().await.map_err(to_app_error)?;
        return Ok(SeedOutcome::default());
    }
    let token = owner_token();
    sqlx::query(INVITE)
        .bind(token.expose())
        .bind(seed.owner_email.as_str())
        .execute(&mut *tx)
        .await
        .map_err(to_app_error)?;
    tx.commit().await.map_err(to_app_error)?;
    Ok(SeedOutcome::with_handover(token))
}

impl TenantHooks for BillingHooks {
    type Seed = BillingSeed;

    fn seed<'a>(
        &'a self,
        _tenant: &'a TenantInfo,
        pool: &'a PgPool,
        seed: &'a BillingSeed,
    ) -> HookFuture<'a, SeedOutcome> {
        Box::pin(seed_tenant(pool, seed))
    }

    fn describe<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, Description> {
        Box::pin(async move {
            let currency: Option<String> =
                sqlx::query_scalar("SELECT currency FROM invoice_series WHERE name = 'default'")
                    .fetch_optional(pool)
                    .await
                    .map_err(to_app_error)?;
            let mut attributes = BTreeMap::new();
            if let Some(currency) = currency {
                attributes.insert("currency".to_owned(), currency);
            }
            Ok(Description {
                display_name: tenant.display_name.clone(),
                attributes,
            })
        })
    }

    fn usage<'a>(
        &'a self,
        _tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, UsageCounters> {
        Box::pin(async move {
            let (invoices, last): (i64, Option<DateTime<Utc>>) =
                sqlx::query_as("SELECT count(*), max(created_at) FROM invoices")
                    .fetch_one(pool)
                    .await
                    .map_err(to_app_error)?;
            Ok(UsageCounters {
                counters: BTreeMap::from([("invoices".to_owned(), invoices)]),
                last_activity: last.map(SystemTime::from),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_prints_no_address_and_tokens_differ() {
        let seed = BillingSeed {
            owner_email: "owner@example.com".to_owned(),
            currency: "EUR".to_owned(),
        };
        let printed = format!("{seed:?}");
        assert!(printed.contains("EUR"), "{printed}");
        assert!(!printed.contains("owner@"), "{printed}");

        let (first, second) = (owner_token(), owner_token());
        assert_eq!(first.expose().len(), 64);
        assert_ne!(first.expose(), second.expose());
    }
}
