//! The App's call authorizer: one synchronous policy hook that every call a
//! component of this process serves passes, keyed by component and method.

use sekvent_context::CallContext;
use sekvent_error::AppError;

use crate::{ComponentDescriptor, MethodDescriptor};

/// The method a served call is for: a component and one of its methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTarget {
    component: &'static ComponentDescriptor,
    index: usize,
}

impl CallTarget {
    /// Method `index` of `component`, in declaration order; `None` when the
    /// component has no such method.
    pub fn new(component: &'static ComponentDescriptor, index: usize) -> Option<Self> {
        (index < component.methods().len()).then_some(Self { component, index })
    }

    /// The component.
    pub const fn component(&self) -> &'static ComponentDescriptor {
        self.component
    }

    /// The method: its name, RPC name and declared flags.
    pub fn method(&self) -> &'static MethodDescriptor {
        &self.component.methods()[self.index]
    }

    /// The method's position among the component's methods.
    pub const fn index(&self) -> usize {
        self.index
    }
}

/// Decides, for every call a component of this App serves, whether it runs.
///
/// Register one per App with
/// [`AppBuilder::call_authorizer`](crate::AppBuilder::call_authorizer). It
/// runs on the serving side only, after authentication and the hop limit and
/// before the request body is read (under gRPC) and before the gate admits
/// the call, under every binding and for every caller: end users, links,
/// anonymous callers and in-process callers (`local`, `local-serialized`,
/// whose context names the trusted caller [`LOCAL_CALLER`](crate::LOCAL_CALLER)
/// and carries the forwarded subject but no end user). A process that only
/// calls a component bound `grpc` never authorizes those calls; the process
/// serving it does. Calls made on an implementation directly, not through a
/// handle, bypass it.
///
/// It is synchronous and must not block: decide from the target and the
/// context alone. A panic is answered like a handler panic.
///
/// Any `Fn(CallTarget, &CallContext) -> Result<(), AppError>` closure is an
/// authorizer.
pub trait CallAuthorizer: Send + Sync + 'static {
    /// `Ok` runs the call; `Err` is the caller's answer, unchanged: the
    /// framework adds no metadata and does not normalise it, so return
    /// `PERMISSION_DENIED` (or `UNAUTHENTICATED`) with the reason and
    /// metadata the caller should see.
    fn authorize(&self, target: CallTarget, cx: &CallContext) -> Result<(), AppError>;

    /// Check that this authorizer knows how to decide the calls of
    /// `component`; an `Err` fails the build. Called once for every
    /// installed component that has an implementation in this binary
    /// (whatever its binding), while the App builds and before any factory
    /// runs. Accepts every component by default.
    fn validate(&self, component: &'static ComponentDescriptor) -> Result<(), AppError> {
        let _ = component;
        Ok(())
    }
}

impl<F> CallAuthorizer for F
where
    F: Fn(CallTarget, &CallContext) -> Result<(), AppError> + Send + Sync + 'static,
{
    fn authorize(&self, target: CallTarget, cx: &CallContext) -> Result<(), AppError> {
        self(target, cx)
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("stock", "Stock").with_idempotent(),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS);

    #[test]
    fn a_target_names_its_component_and_method() {
        let target = CallTarget::new(INVENTORY, 1).unwrap();
        assert_eq!(target.component().name(), "inventory");
        assert_eq!(target.method().name(), "stock");
        assert!(target.method().is_idempotent());
        assert_eq!(target.index(), 1);
        assert_eq!(target, CallTarget::new(INVENTORY, 1).unwrap());
        assert_ne!(target, CallTarget::new(INVENTORY, 0).unwrap());
        assert!(CallTarget::new(INVENTORY, 2).is_none());
        assert!(format!("{target:?}").contains("inventory"));
    }

    #[test]
    fn a_closure_is_an_authorizer_that_accepts_every_component() {
        let authorizer = |target: CallTarget, cx: &CallContext| {
            if target.method().is_idempotent() || cx.subject() == Some("admin") {
                Ok(())
            } else {
                Err(AppError::permission_denied("read only").with_reason("READ_ONLY"))
            }
        };
        let reserve = CallTarget::new(INVENTORY, 0).unwrap();
        let stock = CallTarget::new(INVENTORY, 1).unwrap();
        authorizer
            .authorize(stock, &CallContext::new())
            .expect("reads are allowed");
        authorizer
            .authorize(reserve, &CallContext::new().with_subject("admin"))
            .expect("the admin writes");
        let error = authorizer
            .authorize(reserve, &CallContext::new())
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(error.reason(), Some("READ_ONLY"));
        authorizer.validate(INVENTORY).expect("the default accepts");
    }
}
