//! A lease on a tenant's scope.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use sekvent_db::KeyedRef;

use crate::{TenantId, TenantInfo};

/// What the cache holds for a tenant: the info the scope was built from, and the scope.
pub(super) struct Built<S> {
    /// The row's info as it was committed right before the build; the row named the database,
    /// the role and the role key the scope's pool was opened with.
    pub(super) info: TenantInfo,
    /// The product's scope.
    pub(super) scope: S,
}

/// A tenant's scope, held for as long as it is used. `Debug` prints the id only.
///
/// While a lease (or a clone of it) exists, the cache never evicts the scope; a scope the cache
/// retired meanwhile stays usable until its last lease is dropped. Hold it for the call, not
/// longer.
pub struct TenantLease<S> {
    held: Held<S>,
}

/// The fixed scope, or an entry of the cache.
enum Held<S> {
    Fixed(Arc<Built<S>>),
    Keyed(KeyedRef<Built<S>>),
}

impl<S> TenantLease<S> {
    /// A lease on the fixed form's one scope.
    pub(super) fn fixed(built: Arc<Built<S>>) -> Self {
        Self {
            held: Held::Fixed(built),
        }
    }

    /// A lease on an entry of the cache.
    pub(super) fn keyed(reference: KeyedRef<Built<S>>) -> Self {
        Self {
            held: Held::Keyed(reference),
        }
    }

    fn built(&self) -> &Built<S> {
        match &self.held {
            Held::Fixed(built) => built,
            Held::Keyed(reference) => reference,
        }
    }

    /// The tenant's id.
    pub fn id(&self) -> TenantId {
        self.built().info.id
    }

    /// The info the scope was built from.
    pub fn info(&self) -> &TenantInfo {
        &self.built().info
    }
}

impl<S> Clone for TenantLease<S> {
    fn clone(&self) -> Self {
        let held = match &self.held {
            Held::Fixed(built) => Held::Fixed(Arc::clone(built)),
            Held::Keyed(reference) => Held::Keyed(reference.clone()),
        };
        Self { held }
    }
}

impl<S> Deref for TenantLease<S> {
    type Target = S;

    fn deref(&self) -> &S {
        &self.built().scope
    }
}

// `TenantLease { id: <id>, .. }`
impl<S> fmt::Debug for TenantLease<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantLease")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use crate::{TenantId, TenantInfo, Tenants};

    /// A scope type that derives nothing, `Clone` in particular.
    struct Marker(u32);

    #[tokio::test]
    async fn a_lease_is_cloneable_and_derefs_to_a_scope_that_is_not() {
        let tenants = Tenants::fixed(Marker(7));
        let lease = tenants.get(TenantId::LOCAL).await.expect("the local scope");
        let copy = lease.clone();
        assert_eq!(lease.0, 7);
        assert_eq!(copy.0, 7);
        assert!(
            std::ptr::eq(&raw const *lease, &raw const *copy),
            "clones share one scope"
        );
        assert_eq!(copy.id(), TenantId::LOCAL);
        assert_eq!(copy.info(), &TenantInfo::local());
    }

    #[tokio::test]
    async fn debug_prints_the_id_only() {
        let tenants = Tenants::fixed(Marker(3));
        let lease = tenants.get(TenantId::LOCAL).await.expect("the local scope");
        assert_eq!(
            format!("{lease:?}"),
            "TenantLease { id: 00000000-0000-0000-0000-000000000000, .. }"
        );
    }
}
