//! The public `Invoices` component, on the scope of the call.

use billing_api::{
    CreateInvoiceRequest, Invoice, Invoices, InvoicesError, ListInvoicesReply, ListInvoicesRequest,
};
use sekvent::db::to_app_error;
use sekvent::prelude::*;
use sekvent::tenancy::TenantLease;

use crate::BillingScope;

/// The longest customer name, in characters.
const CUSTOMER_MAX: usize = 200;

/// The page size when the request asks for none.
const DEFAULT_PAGE: u32 = 50;

/// The largest page.
const MAX_PAGE: u32 = 100;

/// The invoices component. It holds nothing: every call reads the tenant's
/// pool from the scope the App's tenant resolver attached to the call.
#[derive(Debug, Default, Clone, Copy)]
pub struct InvoicesService;

/// The tenant scope of the call. Every call the public App serves has one;
/// a call without it (installed in an App without the resolver) is
/// `INTERNAL`.
fn scope(cx: &CallContext) -> Result<&TenantLease<BillingScope>, InvoicesError> {
    cx.scoped::<TenantLease<BillingScope>>()
        .ok_or_else(|| InvoicesError::Other(AppError::internal("the call has no tenant scope")))
}

fn database(error: sqlx::Error) -> InvoicesError {
    InvoicesError::Other(to_app_error(error))
}

fn invalid(field: &str, problem: &str) -> InvoicesError {
    InvoicesError::InvalidInvoice {
        field: field.to_owned(),
        problem: problem.to_owned(),
    }
}

/// The request checked: the customer not blank and not too long, the amount
/// not negative.
fn check(req: &CreateInvoiceRequest) -> Result<(), InvoicesError> {
    if req.customer.trim().is_empty() {
        return Err(invalid("customer", "is blank"));
    }
    if req.customer.chars().count() > CUSTOMER_MAX {
        return Err(invalid("customer", "is longer than 200 characters"));
    }
    if req.amount_cents < 0 {
        return Err(invalid("amount_cents", "is negative"));
    }
    Ok(())
}

/// A page size from 1 to [`MAX_PAGE`]; 0 asks for [`DEFAULT_PAGE`].
fn page_size(requested: u32) -> i64 {
    let size = if requested == 0 {
        DEFAULT_PAGE
    } else {
        requested.min(MAX_PAGE)
    };
    i64::from(size)
}

impl Invoices for InvoicesService {
    async fn create_invoice(
        &self,
        cx: &CallContext,
        req: CreateInvoiceRequest,
    ) -> Result<Invoice, InvoicesError> {
        check(&req)?;
        let scope = scope(cx)?;
        let mut tx = scope.pool.begin().await.map_err(database)?;
        let series: Option<(String, String, i64)> = sqlx::query_as(
            "UPDATE invoice_series SET next_number = next_number + 1 \
             WHERE name = 'default' RETURNING prefix, currency, next_number - 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(database)?;
        let Some((prefix, currency, sequence)) = series else {
            return Err(InvoicesError::NoInvoiceSeries);
        };
        let number = format!("{prefix}{sequence:06}");
        sqlx::query(
            "INSERT INTO invoices (number, customer, amount_cents, currency) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(number.as_str())
        .bind(req.customer.as_str())
        .bind(req.amount_cents)
        .bind(currency.as_str())
        .execute(&mut *tx)
        .await
        .map_err(database)?;
        tx.commit().await.map_err(database)?;
        Ok(Invoice {
            number,
            customer: req.customer,
            amount_cents: req.amount_cents,
            currency,
        })
    }

    async fn list_invoices(
        &self,
        cx: &CallContext,
        req: ListInvoicesRequest,
    ) -> Result<ListInvoicesReply, InvoicesError> {
        let scope = scope(cx)?;
        let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
            "SELECT number, customer, amount_cents, currency FROM invoices ORDER BY id LIMIT $1",
        )
        .bind(page_size(req.page_size))
        .fetch_all(&scope.pool)
        .await
        .map_err(database)?;
        let invoices = rows
            .into_iter()
            .map(|(number, customer, amount_cents, currency)| Invoice {
                number,
                customer,
                amount_cents,
                currency,
            })
            .collect();
        Ok(ListInvoicesReply { invoices })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(customer: &str, amount_cents: i64) -> CreateInvoiceRequest {
        CreateInvoiceRequest {
            customer: customer.to_owned(),
            amount_cents,
        }
    }

    #[test]
    fn requests_are_checked_before_any_database_work() {
        assert!(check(&request("Acme Ltd", 0)).is_ok());
        for (bad, field) in [
            (request(" ", 100), "customer"),
            (request(&"x".repeat(201), 100), "customer"),
            (request("Acme Ltd", -1), "amount_cents"),
        ] {
            let error = check(&bad).expect_err("refused");
            assert!(
                matches!(&error, InvoicesError::InvalidInvoice { field: refused, .. } if refused == field),
                "{error:?}"
            );
        }
    }

    #[test]
    fn page_sizes_are_bounded() {
        assert_eq!(page_size(0), 50);
        assert_eq!(page_size(7), 7);
        assert_eq!(page_size(1_000), 100);
    }

    #[tokio::test]
    async fn a_call_without_a_tenant_scope_is_internal() {
        let error = InvoicesService
            .list_invoices(&CallContext::new(), ListInvoicesRequest { page_size: 0 })
            .await
            .expect_err("no scope");
        let error = AppError::from(error);
        assert_eq!(error.code(), ErrorCode::Internal);
    }
}
