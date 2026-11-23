//! Announcing a committed change of a tenant's row, so that this process and every other one
//! stop answering from what the row held before.

use std::sync::Arc;

use super::hooks::TenantHooks;
use super::provisioner::{Inner, Provisioner};
use crate::TenantId;
use crate::invalidation::ChangeKind;

/// Announces a committed change of `tenant`, in a spawned task the call awaits, so that a caller
/// that goes away (its future dropped after the commit) still announces. In this order: the
/// registry forgets the tenant's lookups; the cache, when the provisioner has one, retires its
/// scope; the provisioner's sink hears `Sink::change`; the sender sends the change to the other
/// processes; the cache's host snapshot is reloaded. A failed reload is a warning (naming the
/// tenant and the error's code, never a host), never the call's error; a task that panicked is
/// a warning too.
///
/// `kind` is `ChangeKind::STATE` for a state change (and for a repeated call that finds the
/// state already set), `ChangeKind::HOSTS` for a change of the hosts.
pub(crate) async fn announce<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tenant: TenantId,
    kind: ChangeKind,
) {
    let inner = Arc::clone(&provisioner.inner);
    let task = tokio::spawn(async move { announce_on(&inner, tenant, &kind).await });
    if let Err(error) = task.await {
        tracing::warn!(
            tenant = %tenant,
            panicked = error.is_panic(),
            "announcing a tenant change did not finish"
        );
    }
}

/// The body of [`announce`], on the spawned task.
async fn announce_on<H: TenantHooks>(inner: &Inner<H>, tenant: TenantId, kind: &ChangeKind) {
    inner.registry.forget(tenant);
    if let Some(cache) = &inner.cache {
        cache.invalidate(tenant).await;
    }
    inner.sink.change(tenant, kind);
    inner.sender.change(tenant, kind);
    if let Some(cache) = &inner.cache
        && let Err(error) = cache.refresh_hosts().await
    {
        tracing::warn!(
            tenant = %tenant,
            code = error.code().as_str(),
            "reloading the host snapshot after a tenant change failed"
        );
    }
}
