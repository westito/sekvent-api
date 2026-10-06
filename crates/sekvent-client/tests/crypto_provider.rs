//! Clients build without a process-wide rustls `CryptoProvider`; only the
//! plain `reqwest_builder` installs one.
//!
//! Its own test binary: no other test in this process can install a
//! default provider before (or while) this one runs.

use rustls::crypto::CryptoProvider;
use sekvent_client::HttpClient;
use sekvent_client::oauth2::ClientCredentials;
use sekvent_config::Secret;

#[test]
fn clients_build_without_installing_a_default_crypto_provider() {
    assert!(CryptoProvider::get_default().is_none());

    HttpClient::builder()
        .base_url("https://billing.example/api")
        .build()
        .unwrap();
    ClientCredentials::builder("https://auth.example/token", "billing", Secret::new("p@ss"))
        .build()
        .unwrap();

    assert!(CryptoProvider::get_default().is_none());

    sekvent_client::reqwest_builder().build().unwrap();
    let installed = CryptoProvider::get_default().unwrap();
    assert_eq!(
        installed.cipher_suites,
        rustls::crypto::ring::default_provider().cipher_suites
    );
}
