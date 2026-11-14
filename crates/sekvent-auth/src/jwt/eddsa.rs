//! `EdDSA` tokens over Ed25519: a private key signs, public keys verify.
//!
//! The pinned `jsonwebtoken` is built without its PEM support, so the two
//! fixed-size encodings are framed here: the 48-byte PKCS#8 v1 private key
//! and the 44-byte `SubjectPublicKeyInfo` public key, each a constant prefix
//! followed by 32 bytes. Private key text and bytes only ever live in
//! borrowed slices of the caller's [`Secret`] or in buffers that wipe
//! themselves; public key text is decoded the same way, since it may be a
//! private key pasted by mistake.
//!
//! The library verifies with `ed25519-dalek`'s lenient `verify` and decodes
//! public keys RFC 8032 refuses, so a public key of small order (which
//! accepts forged signatures) or in a non-canonical encoding is refused here
//! when it is loaded, by comparing bytes.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use sekvent_config::Secret;
use sekvent_error::AppError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::{Claims, TokenRejected, Validation, check_kid, check_unique_kid, verify_with_keys};

/// Length of a raw Ed25519 public key and of a private seed, in bytes.
pub const ED25519_KEY_LEN: usize = 32;

/// PKCS#8 v1 `PrivateKeyInfo` for Ed25519, up to the 32-byte seed.
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const PKCS8_LEN: usize = PKCS8_PREFIX.len() + ED25519_KEY_LEN;
/// Base64 characters of the 48-byte private key: no padding.
const PKCS8_BASE64_LEN: usize = PKCS8_LEN / 3 * 4;

/// `SubjectPublicKeyInfo` for Ed25519, up to the 32-byte public key.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
const SPKI_LEN: usize = SPKI_PREFIX.len() + ED25519_KEY_LEN;
/// Base64 characters of the 44-byte public key, with its one `=`.
const SPKI_BASE64_LEN: usize = SPKI_LEN.div_ceil(3) * 4;
const PRIVATE_LABEL: &str = "PRIVATE KEY";
const PUBLIC_LABEL: &str = "PUBLIC KEY";
const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_END: &str = "-----END ";
const PEM_DASHES: &str = "-----";

/// The y-coordinates of the eight points of small order, little-endian with
/// the sign bit clear.
const SMALL_ORDER_Y: [[u8; ED25519_KEY_LEN]; 5] = [
    // order 1
    [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ],
    // order 2
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // order 4
    [0x00; ED25519_KEY_LEN],
    // order 8
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x05,
    ],
    // order 8
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0x7a,
    ],
];

/// An Ed25519 private key that signs `EdDSA` tokens under one key id.
///
/// Its `Debug` shows the key id only. The public half is not secret:
/// [`public_key_pem`](Self::public_key_pem) gives it to verifiers.
#[derive(Clone)]
pub struct EdDsaSigningKey {
    kid: String,
    signing: EncodingKey,
    public: [u8; ED25519_KEY_LEN],
}

impl EdDsaSigningKey {
    /// From the PKCS#8 PEM of the private key, as
    /// `openssl genpkey -algorithm ed25519` writes it, or its base64 body
    /// alone.
    ///
    /// Fails with `INVALID_ARGUMENT` when `kid` is blank or the text is not
    /// a 48-byte Ed25519 PKCS#8 v1 key (an encrypted key, an `OpenSSH` key,
    /// a key that carries its public half, base64url). The error names the
    /// key id, never the key.
    pub fn from_pkcs8_pem(kid: impl Into<String>, pem: &Secret) -> Result<Self, AppError> {
        let kid = kid.into();
        check_kid(&kid)?;
        let mut der = Zeroizing::new([0_u8; PKCS8_LEN]);
        if !decode_pkcs8_pem(pem.expose(), &mut der) {
            return Err(not_a_private_key(&kid));
        }
        Self::from_pkcs8_der(kid, &der)
    }

    /// From the 32-byte seed (the RFC 8032 private key).
    ///
    /// Deterministic: with [`verification_keys`](Self::verification_keys)
    /// it gives a matching key pair for tests without key files.
    pub fn from_seed(
        kid: impl Into<String>,
        seed: &[u8; ED25519_KEY_LEN],
    ) -> Result<Self, AppError> {
        let kid = kid.into();
        check_kid(&kid)?;
        let mut der = Zeroizing::new([0_u8; PKCS8_LEN]);
        der[..PKCS8_PREFIX.len()].copy_from_slice(&PKCS8_PREFIX);
        der[PKCS8_PREFIX.len()..].copy_from_slice(seed);
        Self::from_pkcs8_der(kid, &der)
    }

    /// The key id new tokens carry.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The raw public key.
    pub fn public_key(&self) -> [u8; ED25519_KEY_LEN] {
        self.public
    }

    /// The public key as `SubjectPublicKeyInfo` PEM, the text
    /// `openssl pkey -pubout` writes.
    pub fn public_key_pem(&self) -> String {
        format!(
            "{PEM_BEGIN}{PUBLIC_LABEL}{PEM_DASHES}\n{}\n{PEM_END}{PUBLIC_LABEL}{PEM_DASHES}\n",
            STANDARD.encode(spki(&self.public))
        )
    }

    /// Keys that verify what this key signs, under its key id.
    pub fn verification_keys(&self) -> EdDsaVerificationKeys {
        EdDsaVerificationKeys {
            verifying: vec![(self.kid.clone(), DecodingKey::from_ed_der(&self.public))],
        }
    }

    /// Sign `claims`, setting `alg: EdDSA` and `kid` in the header.
    pub fn issue<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, AppError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.signing)
            .map_err(|error| AppError::internal(error.to_string()))
    }

    fn from_pkcs8_der(kid: String, der: &[u8; PKCS8_LEN]) -> Result<Self, AppError> {
        if !der.starts_with(&PKCS8_PREFIX) {
            return Err(not_a_private_key(&kid));
        }
        let signing = EncodingKey::from_ed_der(der);
        let Some(public) = derive_public_key(&signing) else {
            return Err(not_a_private_key(&kid));
        };
        Ok(Self {
            kid,
            signing,
            public,
        })
    }
}

impl fmt::Debug for EdDsaSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EdDsaSigningKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

/// Ed25519 public keys that verify `EdDSA` tokens, by key id.
///
/// Built from public keys alone, it can never sign. Its `Debug` shows the
/// key ids only.
#[derive(Clone)]
pub struct EdDsaVerificationKeys {
    verifying: Vec<(String, DecodingKey)>,
}

impl EdDsaVerificationKeys {
    /// Accept tokens signed under `kid` by the key in `pem`: the
    /// `SubjectPublicKeyInfo` PEM that `openssl pkey -pubout` writes, or its
    /// base64 body alone.
    ///
    /// Fails with `INVALID_ARGUMENT` when `kid` is blank, the text is not an
    /// Ed25519 public key, or the key is weak (of small order).
    pub fn from_public_key_pem(kid: impl Into<String>, pem: &str) -> Result<Self, AppError> {
        Self::empty().add(kid.into(), public_key_from_pem(pem))
    }

    /// Accept tokens signed under `kid` by the raw public key `key`.
    ///
    /// Fails on the same conditions as
    /// [`from_public_key_pem`](Self::from_public_key_pem).
    pub fn from_public_key(
        kid: impl Into<String>,
        key: &[u8; ED25519_KEY_LEN],
    ) -> Result<Self, AppError> {
        Self::empty().add(kid.into(), Some(*key))
    }

    /// Also accept tokens signed under `kid` by the key in `pem`.
    ///
    /// Fails on the same conditions as
    /// [`from_public_key_pem`](Self::from_public_key_pem), and when `kid` is
    /// already present.
    pub fn with_public_key_pem(self, kid: impl Into<String>, pem: &str) -> Result<Self, AppError> {
        self.add(kid.into(), public_key_from_pem(pem))
    }

    /// Also accept tokens signed under `kid` by the raw public key `key`.
    ///
    /// Fails on the same conditions as
    /// [`with_public_key_pem`](Self::with_public_key_pem).
    pub fn with_public_key(
        self,
        kid: impl Into<String>,
        key: &[u8; ED25519_KEY_LEN],
    ) -> Result<Self, AppError> {
        self.add(kid.into(), Some(*key))
    }

    /// Every key id accepted.
    pub fn kids(&self) -> impl Iterator<Item = &str> {
        self.verifying.iter().map(|(kid, _)| kid.as_str())
    }

    /// Verify `token` at `now_unix_secs`: `alg: EdDSA`, a known `kid`, the
    /// signature, `exp`, `nbf`, `iat`, then issuer and audience when
    /// `validation` asks for them.
    ///
    /// The same checks and answers as [`JwtKeys::verify`](super::JwtKeys::verify);
    /// a token under any other algorithm is
    /// [`BadSignature`](TokenRejected::BadSignature).
    pub fn verify<T: DeserializeOwned>(
        &self,
        token: &str,
        now_unix_secs: u64,
        validation: &Validation,
    ) -> Result<Claims<T>, TokenRejected> {
        verify_with_keys(
            token,
            Algorithm::EdDSA,
            &self.verifying,
            now_unix_secs,
            validation,
        )
    }

    fn empty() -> Self {
        Self {
            verifying: Vec::new(),
        }
    }

    fn add(mut self, kid: String, key: Option<[u8; ED25519_KEY_LEN]>) -> Result<Self, AppError> {
        check_kid(&kid)?;
        check_unique_kid(&kid, &self.verifying)?;
        let Some(key) = key else {
            return Err(not_a_public_key(&kid));
        };
        let decoding = decoding_key(&kid, &key)?;
        self.verifying.push((kid, decoding));
        Ok(self)
    }
}

impl fmt::Debug for EdDsaVerificationKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EdDsaVerificationKeys")
            .field("kids", &self.kids().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn not_a_private_key(kid: &str) -> AppError {
    AppError::invalid_argument(format!(
        "JWT key `{kid}` is not an Ed25519 private key in PKCS#8 PEM"
    ))
}

fn not_a_public_key(kid: &str) -> AppError {
    AppError::invalid_argument(format!("JWT key `{kid}` is not an Ed25519 public key"))
}

fn weak_public_key(kid: &str) -> AppError {
    AppError::invalid_argument(format!("JWT key `{kid}` is a weak Ed25519 public key"))
}

/// The base64 body of a PEM text, borrowed from it: the lines between the
/// `label` lines when the text starts with `-----BEGIN `, otherwise the
/// whole text.
fn pem_body<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let text = text.trim();
    if !text.starts_with(PEM_BEGIN) {
        return Some(text);
    }
    let first_end = text.find('\n')?;
    let last_start = text.rfind('\n')?;
    let first = &text[..first_end];
    let first = first.strip_suffix('\r').unwrap_or(first);
    let last = &text[last_start + 1..];
    let begin = first.strip_prefix(PEM_BEGIN)?.strip_suffix(PEM_DASHES)?;
    let end = last.strip_prefix(PEM_END)?.strip_suffix(PEM_DASHES)?;
    if begin != label || end != label {
        return None;
    }
    text.get(first_end + 1..last_start)
}

/// Copy the characters of `body` other than ASCII whitespace into `out`,
/// which they must fill exactly. Never writes past `out`.
fn gather<const N: usize>(body: &str, out: &mut [u8; N]) -> bool {
    let mut bytes = body.bytes().filter(|byte| !byte.is_ascii_whitespace());
    for slot in out.iter_mut() {
        let Some(byte) = bytes.next() else {
            return false;
        };
        *slot = byte;
    }
    bytes.next().is_none()
}

/// Decode a PKCS#8 PEM into `der` through a wiped buffer, without
/// allocating. `der` must then still be checked for its prefix.
fn decode_pkcs8_pem(text: &str, der: &mut [u8; PKCS8_LEN]) -> bool {
    let Some(body) = pem_body(text, PRIVATE_LABEL) else {
        return false;
    };
    let mut encoded = Zeroizing::new([0_u8; PKCS8_BASE64_LEN]);
    gather(body, &mut encoded)
        && matches!(
            STANDARD.decode_slice(encoded.as_slice(), der.as_mut_slice()),
            Ok(PKCS8_LEN)
        )
}

/// The public half of a loaded private key, through the library's own
/// derivation.
fn derive_public_key(signing: &EncodingKey) -> Option<[u8; ED25519_KEY_LEN]> {
    let jwk = Jwk::from_encoding_key(signing, Algorithm::EdDSA).ok()?;
    let AlgorithmParameters::OctetKeyPair(params) = &jwk.algorithm else {
        return None;
    };
    let mut public = [0_u8; ED25519_KEY_LEN];
    matches!(
        URL_SAFE_NO_PAD.decode_slice(params.x.as_bytes(), &mut public),
        Ok(ED25519_KEY_LEN)
    )
    .then_some(public)
}

/// The raw key of a `SubjectPublicKeyInfo` PEM or of its base64 body,
/// through wiped fixed buffers and without allocating: the text may be a
/// private key pasted by mistake, and an oversize body is refused once it
/// overflows the buffer.
fn public_key_from_pem(text: &str) -> Option<[u8; ED25519_KEY_LEN]> {
    let body = pem_body(text, PUBLIC_LABEL)?;
    let mut encoded = Zeroizing::new([0_u8; SPKI_BASE64_LEN]);
    let mut der = Zeroizing::new([0_u8; SPKI_LEN]);
    let decoded = gather(body, &mut encoded)
        && matches!(
            STANDARD.decode_slice(encoded.as_slice(), der.as_mut_slice()),
            Ok(SPKI_LEN)
        );
    if !decoded {
        return None;
    }
    let key = der.strip_prefix(&SPKI_PREFIX)?;
    <[u8; ED25519_KEY_LEN]>::try_from(key).ok()
}

fn spki(public: &[u8; ED25519_KEY_LEN]) -> [u8; SPKI_LEN] {
    let mut spki = [0_u8; SPKI_LEN];
    spki[..SPKI_PREFIX.len()].copy_from_slice(&SPKI_PREFIX);
    spki[SPKI_PREFIX.len()..].copy_from_slice(public);
    spki
}

/// A verification key for `key`, after the checks the library does not
/// make: a canonical encoding, not of small order, a point of the curve.
fn decoding_key(kid: &str, key: &[u8; ED25519_KEY_LEN]) -> Result<DecodingKey, AppError> {
    if is_at_or_above_prime(key) {
        return Err(not_a_public_key(kid));
    }
    if is_small_order(key) {
        return Err(weak_public_key(kid));
    }
    let decoding = DecodingKey::from_ed_der(key);
    // An encoding that is no point of the curve fails to build the verifier
    // (`Err`); a key that merely does not match is `Ok(false)`.
    if jsonwebtoken::crypto::verify("", b"", &decoding, Algorithm::EdDSA).is_err() {
        return Err(not_a_public_key(kid));
    }
    Ok(decoding)
}

/// Whether the y-coordinate, sign bit cleared, is at or above the field
/// prime 2^255 - 19 (RFC 8032 section 5.1.3 refuses it).
fn is_at_or_above_prime(key: &[u8; ED25519_KEY_LEN]) -> bool {
    let [first, middle @ .., last] = key;
    *first >= 0xed && middle.iter().all(|&byte| byte == 0xff) && last & 0x7f == 0x7f
}

/// Whether the y-coordinate, sign bit cleared, is that of a point of small
/// order.
fn is_small_order(key: &[u8; ED25519_KEY_LEN]) -> bool {
    let mut y = *key;
    y[ED25519_KEY_LEN - 1] &= 0x7f;
    SMALL_ORDER_Y.contains(&y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{JwtKeys, NoClaims};
    use sekvent_error::ErrorCode;

    // RFC 8032 section 7.1, tests 1 and 2: published test keys.
    const SEED_A: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUBLIC_A: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const SEED_B: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";
    const PUBLIC_B: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
    const PRIVATE_BODY_A: &str = "MC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g";
    const PUBLIC_BODY_A: &str = "MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

    // Signed outside sekvent with pair A under kid `k1`.
    const KNOWN_TOKEN: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFZERTQSIsImtpZCI6ImsxIn0.eyJzdWIiOiJ1c2VyLTEiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6MTcwMDAwMDA2MCwiaXNzIjoiYmlsbGluZyIsImF1ZCI6Im9yZGVycyJ9.KEbNTXyNpaTJgKerHjXhEaJJz4YjprziCf2W8C5Ti7wh89pVlPU0Kcl1JwRetZO1X8VeTqLQkfvFRrEN6zH1BQ";
    const KNOWN_SIGNATURE: &str = "2846cd4d7c8da5a4c980a7ab1e35e111a249cf8623a6bce209fd96f02e538bbc21f3da5594f53429c97527045eb593b55fc55e4ea2d091fbc546b10deb31f505";

    const HS256_SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
    struct Profile {
        name: String,
        roles: Vec<String>,
    }

    fn hex<const N: usize>(text: &str) -> [u8; N] {
        assert_eq!(text.len(), 2 * N);
        let mut out = [0_u8; N];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).unwrap();
        }
        out
    }

    fn signer(kid: &str, seed: &str) -> EdDsaSigningKey {
        EdDsaSigningKey::from_seed(kid, &hex(seed)).unwrap()
    }

    fn private_pem(body: &str, line_end: &str) -> Secret {
        Secret::new(format!(
            "-----BEGIN PRIVATE KEY-----{line_end}{body}{line_end}-----END PRIVATE KEY-----{line_end}"
        ))
    }

    fn public_pem(body: &str, line_end: &str) -> String {
        format!(
            "-----BEGIN PUBLIC KEY-----{line_end}{body}{line_end}-----END PUBLIC KEY-----{line_end}"
        )
    }

    fn known_claims() -> Claims {
        Claims::new("user-1", 1_700_000_000, 60, NoClaims {})
            .with_issuer("billing")
            .with_audience("orders")
    }

    fn known_validation() -> Validation {
        Validation::new()
            .with_issuer("billing")
            .with_audience("orders")
    }

    fn strict() -> Validation {
        Validation::new().with_leeway(0)
    }

    fn segments(token: &str) -> Vec<String> {
        token.split('.').map(str::to_owned).collect()
    }

    fn refusal<T: fmt::Debug>(result: Result<T, AppError>) -> String {
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        error.message().to_owned()
    }

    fn with_sign_bit(mut key: [u8; ED25519_KEY_LEN]) -> [u8; ED25519_KEY_LEN] {
        key[ED25519_KEY_LEN - 1] |= 0x80;
        key
    }

    fn high_y(first: u8, last: u8) -> [u8; ED25519_KEY_LEN] {
        let mut key = [0xff; ED25519_KEY_LEN];
        key[0] = first;
        key[ED25519_KEY_LEN - 1] = last;
        key
    }

    #[test]
    fn known_answer_keys() {
        let a = signer("k1", SEED_A);
        assert_eq!(a.kid(), "k1");
        assert_eq!(a.public_key(), hex::<32>(PUBLIC_A));
        assert_eq!(signer("k2", SEED_B).public_key(), hex::<32>(PUBLIC_B));
        assert_eq!(a.public_key_pem(), public_pem(PUBLIC_BODY_A, "\n"));

        let keys = EdDsaVerificationKeys::from_public_key_pem("k1", &a.public_key_pem()).unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(KNOWN_TOKEN, 1_700_000_030, &known_validation()),
            Ok(known_claims())
        );
    }

    #[test]
    fn known_answer_verifies() {
        let keys = EdDsaVerificationKeys::from_public_key("k1", &hex(PUBLIC_A)).unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(KNOWN_TOKEN, 1_700_000_030, &known_validation()),
            Ok(known_claims())
        );
        assert_eq!(
            segments(KNOWN_TOKEN)[2],
            URL_SAFE_NO_PAD.encode(hex::<64>(KNOWN_SIGNATURE))
        );
    }

    #[test]
    fn known_answer_signs() {
        assert_eq!(
            signer("k1", SEED_A).issue(&known_claims()).unwrap(),
            KNOWN_TOKEN
        );
        let from_pem =
            EdDsaSigningKey::from_pkcs8_pem("k1", &private_pem(PRIVATE_BODY_A, "\n")).unwrap();
        assert_eq!(from_pem.public_key(), hex::<32>(PUBLIC_A));
        assert_eq!(from_pem.issue(&known_claims()).unwrap(), KNOWN_TOKEN);
    }

    #[test]
    fn round_trip_with_and_without_custom_claims() {
        let signer = signer("k1", SEED_A);
        let keys = signer.verification_keys();
        let claims = Claims::new(
            "user-1",
            1_000,
            600,
            Profile {
                name: "Ada".into(),
                roles: vec!["admin".into()],
            },
        )
        .with_issuer("orders")
        .with_audience("web")
        .with_jti("t-1");
        let token = signer.issue(&claims).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.kid.as_deref(), Some("k1"));
        let verified: Claims<Profile> = keys
            .verify(
                &token,
                1_100,
                &Validation::new().with_issuer("orders").with_audience("web"),
            )
            .unwrap();
        assert_eq!(verified, claims);

        let bare = Claims::new("user-1", 1_000, 60, NoClaims {});
        let token = signer.issue(&bare).unwrap();
        assert_eq!(keys.verify::<NoClaims>(&token, 1_000, &strict()), Ok(bare));
    }

    #[test]
    fn pem_forms_load_the_same_key() {
        let split = format!("{}\n{}", &PRIVATE_BODY_A[..40], &PRIVATE_BODY_A[40..]);
        let private = [
            private_pem(PRIVATE_BODY_A, "\n"),
            private_pem(PRIVATE_BODY_A, "\r\n"),
            private_pem(&split, "\r\n"),
            Secret::new(PRIVATE_BODY_A),
            Secret::new(format!("\n  {PRIVATE_BODY_A}  \n")),
            Secret::new(format!(
                "  {}\n",
                private_pem(PRIVATE_BODY_A, "\n").expose()
            )),
        ];
        let verifier = EdDsaVerificationKeys::from_public_key("k1", &hex(PUBLIC_A)).unwrap();
        for pem in &private {
            let token = EdDsaSigningKey::from_pkcs8_pem("k1", pem)
                .unwrap()
                .issue(&known_claims())
                .unwrap();
            assert_eq!(token, KNOWN_TOKEN);
            assert!(
                verifier
                    .verify::<NoClaims>(&token, 1_700_000_030, &known_validation())
                    .is_ok()
            );
        }

        for pem in [
            public_pem(PUBLIC_BODY_A, "\n"),
            public_pem(PUBLIC_BODY_A, "\r\n"),
            PUBLIC_BODY_A.to_owned(),
            format!(" {}\n{} ", &PUBLIC_BODY_A[..30], &PUBLIC_BODY_A[30..]),
        ] {
            let keys = EdDsaVerificationKeys::from_public_key_pem("k1", &pem).unwrap();
            assert!(
                keys.verify::<NoClaims>(KNOWN_TOKEN, 1_700_000_030, &known_validation())
                    .is_ok(),
                "{pem}"
            );
        }
    }

    #[test]
    fn time_checks() {
        let signer = signer("k1", SEED_A);
        let keys = signer.verification_keys();
        let token = signer
            .issue(&Claims::new("u", 1_000, 1_000, NoClaims {}).with_not_before(1_500))
            .unwrap();
        let check = |now, leeway| {
            keys.verify::<NoClaims>(&token, now, &Validation::new().with_leeway(leeway))
                .map(|_| ())
        };
        assert_eq!(check(1_499, 0), Err(TokenRejected::NotYetValid));
        assert_eq!(check(1_500, 0), Ok(()));
        assert_eq!(check(1_489, 10), Err(TokenRejected::NotYetValid));
        assert_eq!(check(1_490, 10), Ok(()));
        assert_eq!(check(1_999, 0), Ok(()));
        assert_eq!(check(2_000, 0), Err(TokenRejected::Expired));
        assert_eq!(check(2_009, 10), Ok(()));
        assert_eq!(check(2_010, 10), Err(TokenRejected::Expired));

        let fresh = signer
            .issue(&Claims::new("u", 1_000, 600, NoClaims {}))
            .unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(&fresh, 999, &strict()),
            Err(TokenRejected::NotYetValid)
        );
        assert!(
            keys.verify::<NoClaims>(&fresh, 990, &Validation::new().with_leeway(10))
                .is_ok()
        );
    }

    #[test]
    fn issuer_and_audience() {
        let signer = signer("k1", SEED_A);
        let keys = signer.verification_keys();
        let bare = signer
            .issue(&Claims::new("u", 1_000, 600, NoClaims {}))
            .unwrap();
        let tagged = signer
            .issue(
                &Claims::new("u", 1_000, 600, NoClaims {})
                    .with_issuer("orders")
                    .with_audience("web"),
            )
            .unwrap();
        assert!(keys.verify::<NoClaims>(&tagged, 1_000, &strict()).is_ok());
        for (token, validation, rejected) in [
            (
                &tagged,
                strict().with_issuer("billing"),
                TokenRejected::WrongIssuer,
            ),
            (
                &bare,
                strict().with_issuer("billing"),
                TokenRejected::WrongIssuer,
            ),
            (
                &tagged,
                strict().with_audience("mobile"),
                TokenRejected::WrongAudience,
            ),
            (
                &bare,
                strict().with_audience("mobile"),
                TokenRejected::WrongAudience,
            ),
        ] {
            assert_eq!(
                keys.verify::<NoClaims>(token, 1_000, &validation),
                Err(rejected)
            );
        }
    }

    #[test]
    fn rotation_by_kid() {
        let a = signer("k1", SEED_A);
        let b = signer("k2", SEED_B);
        let keys = EdDsaVerificationKeys::from_public_key("k1", &a.public_key())
            .unwrap()
            .with_public_key_pem("k2", &b.public_key_pem())
            .unwrap();
        assert_eq!(keys.kids().collect::<Vec<_>>(), ["k1", "k2"]);
        let claims = Claims::new("u", 1_000, 600, NoClaims {});
        for signer in [&a, &b] {
            let token = signer.issue(&claims).unwrap();
            assert!(keys.verify::<NoClaims>(&token, 1_000, &strict()).is_ok());
        }

        let unknown = signer("k3", SEED_A).issue(&claims).unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(&unknown, 1_000, &strict()),
            Err(TokenRejected::UnknownKey)
        );
        let no_kid =
            jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), &claims, &a.signing).unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(&no_kid, 1_000, &strict()),
            Err(TokenRejected::UnknownKey)
        );

        let swapped = EdDsaVerificationKeys::from_public_key("k1", &b.public_key()).unwrap();
        assert_eq!(
            swapped.verify::<NoClaims>(&a.issue(&claims).unwrap(), 1_000, &strict()),
            Err(TokenRejected::BadSignature)
        );
    }

    #[test]
    fn algorithm_confusion_both_ways() {
        let keys = EdDsaVerificationKeys::from_public_key("k1", &hex(PUBLIC_A)).unwrap();
        let claims = Claims::new("u", 1_000, 600, NoClaims {});
        let hs256 = JwtKeys::hs256("k1", &Secret::new(HS256_SECRET)).unwrap();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let public_a: [u8; 32] = hex(PUBLIC_A);
        for token in [
            hs256.issue(&claims).unwrap(),
            jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(&public_a)).unwrap(),
            jsonwebtoken::encode(
                &header,
                &claims,
                &EncodingKey::from_secret(public_pem(PUBLIC_BODY_A, "\n").as_bytes()),
            )
            .unwrap(),
        ] {
            assert_eq!(
                keys.verify::<NoClaims>(&token, 1_000, &strict()),
                Err(TokenRejected::BadSignature)
            );
        }
        assert_eq!(
            hs256.verify::<NoClaims>(KNOWN_TOKEN, 1_700_000_030, &Validation::new()),
            Err(TokenRejected::BadSignature)
        );

        let none = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"none","kid":"k1"}"#),
            URL_SAFE_NO_PAD.encode(r#"{"sub":"admin","iat":1000,"exp":2000}"#),
        );
        assert_eq!(
            keys.verify::<NoClaims>(&none, 1_000, &strict()),
            Err(TokenRejected::Malformed)
        );
    }

    #[test]
    fn tampered_tokens() {
        let signer = signer("k1", SEED_A);
        let keys = signer.verification_keys();
        let user = segments(
            &signer
                .issue(&Claims::new("user", 1_000, 600, NoClaims {}))
                .unwrap(),
        );
        let admin = segments(
            &signer
                .issue(&Claims::new("admin", 1_000, 600, NoClaims {}))
                .unwrap(),
        );

        let spliced = format!("{}.{}.{}", user[0], admin[1], user[2]);
        assert_eq!(
            keys.verify::<NoClaims>(&spliced, 1_000, &strict()),
            Err(TokenRejected::BadSignature)
        );
        let mut signature = user[2].clone();
        let first = if signature.starts_with('A') { "B" } else { "A" };
        signature.replace_range(0..1, first);
        let flipped = format!("{}.{}.{signature}", user[0], user[1]);
        assert_eq!(
            keys.verify::<NoClaims>(&flipped, 1_000, &strict()),
            Err(TokenRejected::BadSignature)
        );
        let short_signature = format!("{}.{}.{}", user[0], user[1], &user[2][..40]);
        assert_eq!(
            keys.verify::<NoClaims>(&short_signature, 1_000, &strict()),
            Err(TokenRejected::BadSignature)
        );
        for truncated in [
            format!("{}.{}.", user[0], user[1]),
            format!("{}.{}", user[0], user[1]),
            KNOWN_TOKEN[..KNOWN_TOKEN.len() - 1].to_owned(),
        ] {
            assert!(
                keys.verify::<NoClaims>(&truncated, 1_000, &strict())
                    .is_err(),
                "{truncated}"
            );
        }

        let unsigned_claims = serde_json::json!({"sub": "u", "iat": 1_000});
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("k1".into());
        let missing_exp = jsonwebtoken::encode(&header, &unsigned_claims, &signer.signing).unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(&missing_exp, 1_000, &strict()),
            Err(TokenRejected::Malformed)
        );
    }

    #[test]
    fn private_key_refusals() {
        let der: [u8; PKCS8_LEN] = {
            let mut der = [0_u8; PKCS8_LEN];
            der[..PKCS8_PREFIX.len()].copy_from_slice(&PKCS8_PREFIX);
            der[PKCS8_PREFIX.len()..].copy_from_slice(&hex::<32>(SEED_A));
            der
        };
        assert_eq!(STANDARD.encode(der), PRIVATE_BODY_A);
        let mut wrong_prefix = der;
        wrong_prefix[4] = 0x01;
        let mut with_public = der.to_vec();
        with_public[1] = 0x51;
        with_public[4] = 0x01;
        with_public.extend_from_slice(&[0x81, 0x21, 0x00]);
        with_public.extend_from_slice(&hex::<32>(PUBLIC_A));
        let mut longer = der.to_vec();
        longer.push(0);

        let inputs = [
            private_pem(PRIVATE_BODY_A, "\n")
                .expose()
                .replace("PRIVATE KEY", "RSA PRIVATE KEY"),
            private_pem(PRIVATE_BODY_A, "\n")
                .expose()
                .replace("PRIVATE KEY", "ENCRYPTED PRIVATE KEY"),
            private_pem(PRIVATE_BODY_A, "\n")
                .expose()
                .replacen("PRIVATE KEY", "PUBLIC KEY", 1),
            format!("-----BEGIN PRIVATE KEY-----\n{PRIVATE_BODY_A}"),
            "-----BEGIN PRIVATE KEY-----".to_owned(),
            "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----".to_owned(),
            "!".repeat(PKCS8_BASE64_LEN),
            PRIVATE_BODY_A.replace('/', "_"),
            STANDARD.encode(&der[..PKCS8_LEN - 1]),
            STANDARD.encode(&longer),
            STANDARD.encode(wrong_prefix),
            STANDARD.encode(&with_public),
            public_pem(PUBLIC_BODY_A, "\n"),
            PUBLIC_BODY_A.to_owned(),
            PRIVATE_BODY_A[..PKCS8_BASE64_LEN - 1].to_owned(),
            format!("{PRIVATE_BODY_A}A"),
            String::new(),
        ];
        for input in inputs {
            let message = refusal(EdDsaSigningKey::from_pkcs8_pem(
                "bad-key",
                &Secret::new(input.clone()),
            ));
            assert_eq!(
                message, "JWT key `bad-key` is not an Ed25519 private key in PKCS#8 PEM",
                "{input}"
            );
        }

        let pem = private_pem(PRIVATE_BODY_A, "\n");
        assert_eq!(
            refusal(EdDsaSigningKey::from_pkcs8_pem(" ", &pem)),
            "JWT key id must not be blank"
        );
        assert_eq!(
            refusal(EdDsaSigningKey::from_seed("", &hex(SEED_A))),
            "JWT key id must not be blank"
        );
    }

    #[test]
    fn public_key_refusals() {
        let not_a_point: [u8; 32] = {
            let mut key = [0_u8; 32];
            key[0] = 0x02;
            key
        };
        let spki_a = spki(&hex(PUBLIC_A));
        let mut wrong_prefix = spki_a;
        wrong_prefix[2] = 0x31;
        let mut longer = spki_a.to_vec();
        longer.push(0);
        let expected = "JWT key `bad-key` is not an Ed25519 public key";
        assert_eq!(PUBLIC_BODY_A.len(), SPKI_BASE64_LEN);
        for input in [
            private_pem(PRIVATE_BODY_A, "\n").expose().to_owned(),
            private_pem(PRIVATE_BODY_A, "\n")
                .expose()
                .replace("PRIVATE KEY", "PUBLIC KEY"),
            PRIVATE_BODY_A.to_owned(),
            "A".repeat(1 << 20),
            public_pem(&"A".repeat(1 << 20), "\n"),
            public_pem(&format!("{PUBLIC_BODY_A}A"), "\n"),
            PUBLIC_BODY_A.trim_end_matches('=').to_owned(),
            public_pem(PUBLIC_BODY_A, "\n").replace("PUBLIC KEY", "RSA PUBLIC KEY"),
            "-----BEGIN PUBLIC KEY-----".to_owned(),
            "!".repeat(60),
            PUBLIC_BODY_A.replace('/', "_"),
            STANDARD.encode(&spki_a[..SPKI_LEN - 1]),
            STANDARD.encode(&longer),
            STANDARD.encode(wrong_prefix),
            STANDARD.encode(spki(&not_a_point)),
            String::new(),
        ] {
            assert_eq!(
                refusal(EdDsaVerificationKeys::from_public_key_pem(
                    "bad-key", &input
                )),
                expected,
                "{input}"
            );
        }
        assert_eq!(
            refusal(EdDsaVerificationKeys::from_public_key(
                "bad-key",
                &not_a_point
            )),
            expected
        );

        let keys = EdDsaVerificationKeys::from_public_key("k1", &hex(PUBLIC_A)).unwrap();
        assert_eq!(
            refusal(keys.clone().with_public_key("k1", &hex(PUBLIC_B))),
            "JWT key id `k1` is configured twice"
        );
        assert_eq!(
            refusal(keys.clone().with_public_key_pem("bad-key", "garbage")),
            expected
        );
        assert_eq!(
            refusal(keys.with_public_key(" ", &hex(PUBLIC_B))),
            "JWT key id must not be blank"
        );
        assert_eq!(
            refusal(EdDsaVerificationKeys::from_public_key_pem(
                "",
                PUBLIC_BODY_A
            )),
            "JWT key id must not be blank"
        );
    }

    #[test]
    fn weak_and_non_canonical_public_keys_are_refused() {
        let weak: Vec<[u8; 32]> = SMALL_ORDER_Y
            .iter()
            .flat_map(|&y| [y, with_sign_bit(y)])
            .collect();
        let at_or_above_prime = [
            high_y(0xed, 0x7f),
            high_y(0xed, 0xff),
            high_y(0xee, 0x7f),
            high_y(0xee, 0xff),
            high_y(0xff, 0x7f),
        ];
        let cases: Vec<_> = weak
            .iter()
            .map(|key| (key, "JWT key `bad-key` is a weak Ed25519 public key"))
            .chain(
                at_or_above_prime
                    .iter()
                    .map(|key| (key, "JWT key `bad-key` is not an Ed25519 public key")),
            )
            .collect();
        // The fourteen small-order encodings, and one more y above the prime.
        assert_eq!(cases.len(), 15);
        let keys = EdDsaVerificationKeys::from_public_key("k1", &hex(PUBLIC_A)).unwrap();
        for (key, expected) in cases {
            assert_eq!(
                refusal(EdDsaVerificationKeys::from_public_key("bad-key", key)),
                expected
            );
            assert_eq!(
                refusal(EdDsaVerificationKeys::from_public_key_pem(
                    "bad-key",
                    &public_pem(&STANDARD.encode(spki(key)), "\n"),
                )),
                expected
            );
            assert_eq!(
                refusal(keys.clone().with_public_key("bad-key", key)),
                expected
            );
        }
        assert!(EdDsaVerificationKeys::from_public_key("a", &hex(PUBLIC_A)).is_ok());
        assert!(EdDsaVerificationKeys::from_public_key("b", &hex(PUBLIC_B)).is_ok());
        for byte in (0..=u8::MAX).step_by(8) {
            let signer = EdDsaSigningKey::from_seed("k", &[byte; 32]).unwrap();
            assert!(
                EdDsaVerificationKeys::from_public_key("k", &signer.public_key()).is_ok(),
                "seed {byte}"
            );
            assert!(
                EdDsaVerificationKeys::from_public_key_pem("k", &signer.public_key_pem()).is_ok()
            );
        }
    }

    #[test]
    fn debug_shows_kids_only() {
        let signer = signer("k1", SEED_A);
        assert_eq!(
            format!("{signer:?}"),
            r#"EdDsaSigningKey { kid: "k1", .. }"#
        );
        let keys = signer
            .verification_keys()
            .with_public_key("k2", &hex(PUBLIC_B))
            .unwrap();
        assert_eq!(
            format!("{keys:?}"),
            r#"EdDsaVerificationKeys { kids: ["k1", "k2"], .. }"#
        );
        assert_eq!(keys.kids().collect::<Vec<_>>(), ["k1", "k2"]);
    }
}
