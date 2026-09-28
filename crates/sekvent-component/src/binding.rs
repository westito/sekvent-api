use std::fmt;

/// How calls to a component travel from caller to implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Binding {
    /// A direct call into the implementation, in the caller's task, with no
    /// encoding.
    Local,
    /// In-process, but across a serialization boundary: the request, the
    /// call context and the outcome are encoded and decoded, and the call runs
    /// on its own task, as it would behind a network transport.
    LocalSerialized,
    /// A gRPC channel to another process. Parsed, but not available in this
    /// build.
    Grpc,
}

impl Binding {
    /// Every binding, in declaration order.
    pub const ALL: [Binding; 3] = [Self::Local, Self::LocalSerialized, Self::Grpc];

    /// The configuration value: `local`, `local-serialized` or `grpc`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LocalSerialized => "local-serialized",
            Self::Grpc => "grpc",
        }
    }

    /// Parse a configuration value. Only the exact [`as_str`](Self::as_str)
    /// spellings are accepted: no trimming, no other case.
    pub fn parse(value: &str) -> Option<Binding> {
        Self::ALL
            .into_iter()
            .find(|binding| binding.as_str() == value)
    }

    /// Whether the implementation runs in this process
    /// ([`Local`](Self::Local) or [`LocalSerialized`](Self::LocalSerialized)).
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Local | Self::LocalSerialized)
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a component may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComponentMode {
    /// Any binding; requests and replies are protobuf messages.
    Standard,
    /// Only the `local` binding; requests and replies may be plain Rust types.
    LocalOnly,
    /// Never runs in this binary; it must be bound to a remote transport.
    RemoteOnly,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_round_trips_through_its_text() {
        for binding in Binding::ALL {
            assert_eq!(Binding::parse(binding.as_str()), Some(binding));
            assert_eq!(binding.to_string(), binding.as_str());
        }
        assert_eq!(
            Binding::ALL.map(Binding::as_str),
            ["local", "local-serialized", "grpc"]
        );
    }

    #[test]
    fn only_exact_spellings_parse() {
        for value in [
            "",
            "Local",
            " local",
            "local ",
            "LOCAL",
            "serialized",
            "local_serialized",
            "gRPC",
        ] {
            assert_eq!(Binding::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn local_bindings_are_local() {
        assert!(Binding::Local.is_local());
        assert!(Binding::LocalSerialized.is_local());
        assert!(!Binding::Grpc.is_local());
    }

    #[test]
    fn modes_compare() {
        assert_ne!(ComponentMode::Standard, ComponentMode::LocalOnly);
        assert_eq!(format!("{:?}", ComponentMode::RemoteOnly), "RemoteOnly");
    }
}
