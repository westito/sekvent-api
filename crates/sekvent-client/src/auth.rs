use futures::future::BoxFuture;
use sekvent_config::Secret;
use sekvent_context::CallContext;
use sekvent_error::AppError;

/// A source of bearer tokens attached as `Authorization: Bearer …` to every
/// request of an [`HttpClient`](crate::HttpClient).
///
/// When a request is answered with `401`, the client calls
/// [`BearerSource::invalidate`], fetches a fresh token and sends the request
/// once more — also for non-idempotent methods, since a `401` means the
/// request was not processed.
pub trait BearerSource: Send + Sync + 'static {
    /// A currently valid token (cached or freshly fetched).
    fn token<'a>(&'a self, ctx: &'a CallContext) -> BoxFuture<'a, Result<Secret, AppError>>;

    /// Forget the cached token; the next [`BearerSource::token`] fetches anew.
    fn invalidate(&self);
}
