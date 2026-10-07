use std::sync::Arc;

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;

use crate::BuildError;

/// The rustls configuration every client in this crate hands to `reqwest`.
///
/// The ring provider is passed explicitly, so building a client neither
/// needs nor installs a process-wide default `CryptoProvider`. Certificates
/// are checked by the platform verifier, and ALPN offers HTTP/2 before
/// HTTP/1.1; both match what `reqwest` sets up for its own rustls backend.
fn client_config() -> Result<ClientConfig, BuildError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(BuildError::tls)?
        .with_platform_verifier()
        .map_err(BuildError::tls)?
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// The `reqwest` builder the clients of this crate start from.
///
/// The TLS backend is preconfigured, so `reqwest`'s own TLS settings
/// (`tls_*`, `identity`, ALPN from `http1_only`) would be ignored; the
/// clients here set none of them.
pub(crate) fn internal_builder() -> Result<reqwest::ClientBuilder, BuildError> {
    Ok(reqwest::Client::builder().tls_backend_preconfigured(client_config()?))
}

/// A plain `reqwest` builder for code that cannot use
/// [`HttpClient`](crate::HttpClient), such as tests or an upstream that
/// needs its own TLS settings.
///
/// sekvent pins `reqwest` without a built-in crypto provider, so a bare
/// `reqwest::Client::new()` panics. This installs ring as the process-wide
/// default rustls provider unless one is installed already (an installed
/// provider is kept), then returns `reqwest`'s own rustls builder: every
/// `reqwest` TLS setting (`tls_certs_only`, `tls_version_min`, `identity`,
/// `http1_only`, …) applies as documented.
pub fn reqwest_builder() -> reqwest::ClientBuilder {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().tls_backend_rustls()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_uses_ring_and_offers_http2_first() {
        let config = client_config().unwrap();
        assert_eq!(
            config.crypto_provider().cipher_suites,
            rustls::crypto::ring::default_provider().cipher_suites
        );
        assert!(
            config
                .crypto_provider()
                .kx_groups
                .iter()
                .all(|group| group.name() != rustls::NamedGroup::X25519MLKEM768)
        );
        assert_eq!(
            config.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(config.enable_sni);
    }
}
