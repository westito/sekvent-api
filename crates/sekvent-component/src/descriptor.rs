use std::fmt;
use std::time::Duration;

use crate::ComponentMode;

/// The kind of a component method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MethodKind {
    /// Request and reply: the caller waits for the outcome.
    Call,
}

/// Static description of one component method, as declared on the trait.
///
/// Built with `const` builders so generated code keeps compiling when fields
/// are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDescriptor {
    name: &'static str,
    rpc: &'static str,
    kind: MethodKind,
    idempotent: bool,
    timeout: Option<Duration>,
    bulkhead: Option<u32>,
}

impl MethodDescriptor {
    /// A [`MethodKind::Call`] method: `name` is the Rust method name
    /// (`reserve`), `rpc` its RPC name (`Reserve`). Not idempotent, no
    /// timeout, no bulkhead.
    pub const fn call(name: &'static str, rpc: &'static str) -> Self {
        Self {
            name,
            rpc,
            kind: MethodKind::Call,
            idempotent: false,
            timeout: None,
            bulkhead: None,
        }
    }

    /// Mark the method safe to repeat.
    #[must_use]
    pub const fn with_idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }

    /// Default time limit of one call.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Default cap on concurrent calls.
    #[must_use]
    pub const fn with_bulkhead(mut self, max_concurrent: u32) -> Self {
        self.bulkhead = Some(max_concurrent);
        self
    }

    /// The Rust method name, e.g. `reserve`.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The RPC name, e.g. `Reserve`.
    pub const fn rpc(&self) -> &'static str {
        self.rpc
    }

    /// The method kind.
    pub const fn kind(&self) -> MethodKind {
        self.kind
    }

    /// Whether the method is declared idempotent.
    pub const fn is_idempotent(&self) -> bool {
        self.idempotent
    }

    /// The declared timeout, if any.
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The declared bulkhead size, if any.
    pub const fn bulkhead(&self) -> Option<u32> {
        self.bulkhead
    }
}

/// Static description of a component, as declared by `#[component]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentDescriptor {
    name: &'static str,
    service: &'static str,
    package: Option<&'static str>,
    mode: ComponentMode,
    methods: &'static [MethodDescriptor],
}

impl ComponentDescriptor {
    /// A [`ComponentMode::Standard`] component without a package: `name` is
    /// the component name (`inventory`), `service` the trait name
    /// (`Inventory`).
    pub const fn new(
        name: &'static str,
        service: &'static str,
        methods: &'static [MethodDescriptor],
    ) -> Self {
        Self {
            name,
            service,
            package: None,
            mode: ComponentMode::Standard,
            methods,
        }
    }

    /// Set the protobuf package, e.g. `shop.inventory.v1`.
    #[must_use]
    pub const fn with_package(mut self, package: &'static str) -> Self {
        self.package = Some(package);
        self
    }

    /// Set the mode.
    #[must_use]
    pub const fn with_mode(mut self, mode: ComponentMode) -> Self {
        self.mode = mode;
        self
    }

    /// The component name, e.g. `inventory`.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The service (trait) name, e.g. `Inventory`.
    pub const fn service(&self) -> &'static str {
        self.service
    }

    /// The protobuf package, if declared.
    pub const fn package(&self) -> Option<&'static str> {
        self.package
    }

    /// The mode.
    pub const fn mode(&self) -> ComponentMode {
        self.mode
    }

    /// The methods, in declaration order; a method's index is its position.
    pub const fn methods(&self) -> &'static [MethodDescriptor] {
        self.methods
    }

    /// `<package>.<service>`, e.g. `shop.inventory.v1.Inventory`; `None`
    /// without a package.
    pub fn full_service_name(&self) -> Option<String> {
        self.package
            .map(|package| format!("{package}.{}", self.service))
    }
}

/// Implemented by every generated component handle.
pub trait ComponentHandle: Clone + fmt::Debug + Send + Sync + 'static {
    /// The component this handle calls.
    const DESCRIPTOR: &'static ComponentDescriptor;
}

#[cfg(test)]
mod tests {
    use super::*;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve")
            .with_idempotent()
            .with_timeout(Duration::from_secs(2))
            .with_bulkhead(16),
        MethodDescriptor::call("release", "Release"),
    ];

    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS)
            .with_package("shop.inventory.v1");

    #[test]
    fn method_builders_and_getters() {
        let reserve = METHODS[0];
        assert_eq!(reserve.name(), "reserve");
        assert_eq!(reserve.rpc(), "Reserve");
        assert_eq!(reserve.kind(), MethodKind::Call);
        assert!(reserve.is_idempotent());
        assert_eq!(reserve.timeout(), Some(Duration::from_secs(2)));
        assert_eq!(reserve.bulkhead(), Some(16));

        let release = METHODS[1];
        assert!(!release.is_idempotent());
        assert_eq!(release.timeout(), None);
        assert_eq!(release.bulkhead(), None);
        assert_ne!(reserve, release);
    }

    #[test]
    fn component_builders_and_getters() {
        assert_eq!(INVENTORY.name(), "inventory");
        assert_eq!(INVENTORY.service(), "Inventory");
        assert_eq!(INVENTORY.package(), Some("shop.inventory.v1"));
        assert_eq!(INVENTORY.mode(), ComponentMode::Standard);
        assert_eq!(INVENTORY.methods().len(), 2);
        assert_eq!(
            INVENTORY.full_service_name().as_deref(),
            Some("shop.inventory.v1.Inventory")
        );

        let notes =
            ComponentDescriptor::new("notes", "Notes", &[]).with_mode(ComponentMode::LocalOnly);
        assert_eq!(notes.mode(), ComponentMode::LocalOnly);
        assert_eq!(notes.package(), None);
        assert_eq!(notes.full_service_name(), None);
        assert!(format!("{notes:?}").contains("LocalOnly"));
    }
}
