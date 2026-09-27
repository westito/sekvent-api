use crate::token::MIN_TOKEN_LEN;

/// A service-link setup error. Messages name links, never tokens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// A link without a name.
    #[error("a service link name must not be blank")]
    BlankName,
    /// The token is empty or whitespace only.
    #[error("service link `{link}` has a blank token")]
    BlankToken {
        /// The link.
        link: String,
    },
    /// The token is shorter than [`MIN_TOKEN_LEN`].
    #[error("service link `{link}` has a token shorter than {MIN_TOKEN_LEN} characters")]
    TooShort {
        /// The link.
        link: String,
    },
    /// The token contains whitespace or a character outside `[A-Za-z0-9_-]`.
    #[error(
        "service link `{link}` has a non-canonical token: only A-Z, a-z, 0-9, `-` and `_` are \
         allowed, without whitespace"
    )]
    NonCanonical {
        /// The link.
        link: String,
    },
    /// Two links share one token, so the token cannot identify the caller.
    #[error("service links `{first}` and `{second}` use the same token")]
    DuplicateToken {
        /// The link seen first.
        first: String,
        /// The link seen second.
        second: String,
    },
    /// The same link name appears twice.
    #[error("service link `{link}` is configured twice")]
    DuplicateName {
        /// The link.
        link: String,
    },
    /// The operating system's random source failed.
    #[error("the operating system random source failed")]
    Randomness,
}
